//! Safe projections of peer-controlled text.
//!
//! A peer chooses device names, file names, and text previews. Hyakume keeps
//! those original bytes in [`PeerText`] and never prints them directly.
//! Output goes through a projection:
//!
//! - [`SafeLine`] for single-line fields. Control characters, line breaks,
//!   bidirectional and invisible formatting characters, quotes, and
//!   backslashes become visible escapes, and the result stops at a column
//!   limit with a visible marker.
//! - [`SafeBlock`] for multi-line previews. Each line is a [`SafeLine`], and
//!   the block stops at a byte and line limit.
//!
//! Both read their input lazily and stop at their limit, so the work and
//! memory depend on the limit, not on the input size.

use std::fmt;

use unicode_width::UnicodeWidthChar;

/// Marker written after a projection that was cut at its limit.
pub const SHORTENED: &str = " [shortened]";

/// Columns used when a [`PeerText`] is formatted with `{:?}`, such as in
/// diagnostics.
const DEBUG_COLUMNS: usize = 64;

/// Original bytes supplied by a peer, bounded at construction.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct PeerText(Vec<u8>);

/// The peer value was longer than its limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("peer text is {len} bytes; the limit is {limit}")]
pub struct TooLong {
    pub len: usize,
    pub limit: usize,
}

impl PeerText {
    pub fn new(bytes: Vec<u8>, limit: usize) -> Result<Self, TooLong> {
        if bytes.len() > limit {
            return Err(TooLong {
                len: bytes.len(),
                limit,
            });
        }
        Ok(Self(bytes))
    }

    /// The original bytes. Never print these; use [`PeerText::line`].
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// A single-line projection at most `columns` wide.
    pub fn line(&self, columns: usize) -> SafeLine {
        SafeLine::project(&self.0, columns)
    }
}

impl fmt::Debug for PeerText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PeerText({})", self.line(DEBUG_COLUMNS))
    }
}

/// A single line of peer text that is safe to show, shown in double quotes.
#[derive(Clone, PartialEq, Eq)]
pub struct SafeLine {
    text: String,
    shortened: bool,
}

impl SafeLine {
    /// Projects `raw` into at most `columns` terminal columns, not counting
    /// the quotes and the shortened marker.
    pub fn project(raw: &[u8], columns: usize) -> Self {
        let mut text = String::new();
        let mut used = 0usize;
        let mut shortened = false;
        'outer: for chunk in raw.utf8_chunks() {
            for c in chunk.valid().chars() {
                let piece = Escaped::of(c);
                let width = piece.width();
                if used + width > columns {
                    shortened = true;
                    break 'outer;
                }
                piece.push_to(&mut text);
                used += width;
            }
            for byte in chunk.invalid() {
                if used + 4 > columns {
                    shortened = true;
                    break 'outer;
                }
                text.push_str(&format!("\\x{byte:02x}"));
                used += 4;
            }
        }
        Self { text, shortened }
    }

    /// The escaped text without quotes.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn is_shortened(&self) -> bool {
        self.shortened
    }
}

impl fmt::Display for SafeLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"{}\"", self.text)?;
        if self.shortened {
            f.write_str(SHORTENED)?;
        }
        Ok(())
    }
}

impl fmt::Debug for SafeLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A bounded multi-line preview of peer text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafeBlock {
    lines: Vec<SafeLine>,
    shortened: bool,
}

impl SafeBlock {
    /// Projects at most `max_bytes` of `raw`, split on `\n`, into at most
    /// `max_lines` lines of at most `columns` columns each. The block is
    /// shortened when any of those limits cut something off.
    pub fn project(raw: &[u8], max_bytes: usize, max_lines: usize, columns: usize) -> Self {
        let mut shortened = raw.len() > max_bytes;
        let mut head = &raw[..raw.len().min(max_bytes)];
        if shortened {
            // Do not show a character cut in half as escaped bytes.
            if let Err(e) = std::str::from_utf8(head)
                && e.error_len().is_none()
            {
                head = &head[..e.valid_up_to()];
            }
        }
        let mut lines = Vec::new();
        for (index, line) in head.split(|b| *b == b'\n').enumerate() {
            if index == max_lines {
                shortened = true;
                break;
            }
            let line = SafeLine::project(line, columns);
            shortened |= line.is_shortened();
            lines.push(line);
        }
        Self { lines, shortened }
    }

    pub fn lines(&self) -> &[SafeLine] {
        &self.lines
    }

    pub fn is_shortened(&self) -> bool {
        self.shortened
    }
}

/// How one character appears in a projection.
enum Escaped {
    Plain(char),
    Text(String),
}

impl Escaped {
    fn of(c: char) -> Self {
        match c {
            '\\' => Self::Text("\\\\".into()),
            '"' => Self::Text("\\\"".into()),
            '\n' => Self::Text("\\n".into()),
            '\r' => Self::Text("\\r".into()),
            '\t' => Self::Text("\\t".into()),
            // Spaces other than U+0020 look like a space but are not one.
            c if c.is_control() || is_hidden_format(c) || (c.is_whitespace() && c != ' ') => {
                let code = u32::from(c);
                if code <= 0xff {
                    Self::Text(format!("\\x{code:02x}"))
                } else {
                    Self::Text(format!("\\u{{{code:x}}}"))
                }
            }
            c => Self::Plain(c),
        }
    }

    fn width(&self) -> usize {
        match self {
            // Characters without a defined width (none reach here after the
            // control check) count as one column to stay conservative.
            Self::Plain(c) => c.width().unwrap_or(1),
            Self::Text(s) => s.len(),
        }
    }

    fn push_to(&self, out: &mut String) {
        match self {
            Self::Plain(c) => out.push(*c),
            Self::Text(s) => out.push_str(s),
        }
    }
}

/// Characters that reorder, hide, or join text without being control
/// characters: bidirectional controls, zero-width characters, separators,
/// fillers, variation selectors, and tags.
fn is_hidden_format(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{034f}'
            | '\u{061c}'
            | '\u{115f}'..='\u{1160}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{feff}'
            | '\u{ffa0}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0000}'..='\u{e0fff}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(raw: &str, columns: usize) -> String {
        SafeLine::project(raw.as_bytes(), columns).to_string()
    }

    #[test]
    fn terminal_controls_and_line_breaks_become_visible_escapes() {
        let out = line("a\x1b[2Jb\nc\rd\u{7}e\u{9b}f", 80);
        assert_eq!(out, "\"a\\x1b[2Jb\\nc\\rd\\x07e\\x9bf\"");
        assert!(!out.chars().any(char::is_control));
    }

    #[test]
    fn bidirectional_and_invisible_characters_are_exposed() {
        // "photo<RLO>gpj.exe" displays as "photoexe.jpg" when interpreted.
        let out = line("photo\u{202e}gpj.exe\u{200b}\u{2066}", 80);
        assert_eq!(out, "\"photo\\u{202e}gpj.exe\\u{200b}\\u{2066}\"");
    }

    #[test]
    fn quotes_and_backslashes_cannot_end_the_field_early() {
        let out = line("a\" Accept? [y/N] \"\\", 80);
        // Every quote and backslash inside is escaped, so the field still
        // ends at the final quote.
        assert_eq!(out, "\"a\\\" Accept? [y/N] \\\"\\\\\"");
    }

    #[test]
    fn spaces_that_only_look_like_spaces_are_escaped() {
        let out = line("a\u{a0}b\u{2003}c\u{3000}d e", 80);
        assert_eq!(out, "\"a\\xa0b\\u{2003}c\\u{3000}d e\"");
    }

    #[test]
    fn invalid_utf8_is_shown_as_byte_escapes() {
        let out = SafeLine::project(b"ok\xff\xfe!", 80).to_string();
        assert_eq!(out, "\"ok\\xff\\xfe!\"");
    }

    #[test]
    fn long_and_wide_text_stops_at_the_column_limit_with_a_marker() {
        let wide = "漢".repeat(100);
        let projected = SafeLine::project(wide.as_bytes(), 120);
        assert!(projected.is_shortened());
        assert_eq!(projected.as_str().chars().count(), 60);
        assert!(projected.to_string().ends_with(SHORTENED));

        let exact = SafeLine::project("abc".as_bytes(), 3);
        assert!(!exact.is_shortened());
        assert_eq!(exact.to_string(), "\"abc\"");
    }

    #[test]
    fn an_escape_that_would_cross_the_limit_is_left_out_whole() {
        let projected = SafeLine::project("ab\x1b".as_bytes(), 5);
        assert_eq!(projected.as_str(), "ab");
        assert!(projected.is_shortened());
    }

    #[test]
    fn one_column_never_holds_part_of_an_escape() {
        let space = SafeLine::project("\u{a0}".as_bytes(), 1);
        assert_eq!(space.as_str(), "");
        assert!(space.is_shortened());
        let controls = SafeLine::project(b" \x7f\x00\x0b", 1);
        assert_eq!(controls.as_str(), " ");
        assert!(controls.is_shortened());
    }

    #[test]
    fn previews_stop_at_their_line_and_byte_limits() {
        let text = "one\ntwo\x1b[31m\nthree\nfour";
        let block = SafeBlock::project(text.as_bytes(), 4096, 3, 80);
        let lines: Vec<String> = block.lines().iter().map(ToString::to_string).collect();
        assert_eq!(lines, ["\"one\"", "\"two\\x1b[31m\"", "\"three\""]);
        assert!(block.is_shortened());

        let cut = SafeBlock::project("aé".as_bytes(), 2, 10, 80);
        assert_eq!(cut.lines()[0].as_str(), "a");
        assert!(cut.is_shortened());
    }

    #[test]
    fn debug_formatting_of_peer_text_is_bounded_and_escaped() {
        let hostile = PeerText::new(format!("x\n{}", "y".repeat(5000)).into_bytes(), 8192)
            .expect("within limit");
        let printed = format!("{hostile:?}");
        assert!(printed.len() < 128, "{}", printed.len());
        assert!(!printed.contains('\n'));
        assert!(printed.contains(SHORTENED));
    }

    #[test]
    fn peer_text_over_its_limit_is_refused() {
        assert_eq!(
            PeerText::new(vec![b'a'; 5], 4),
            Err(TooLong { len: 5, limit: 4 })
        );
        assert!(PeerText::new(vec![b'a'; 4], 4).is_ok());
    }
}
