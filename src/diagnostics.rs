//! Bounded, redacted diagnostics.
//!
//! Diagnostics are off unless `HYAKUME_LOG` names a level (`error`, `warn`,
//! `info`, `debug`, or `trace`). They go to standard error without colour
//! codes.
//!
//! Two rules keep secrets and peer-controlled text out of the output:
//!
//! - Secret or payload values are logged only through [`Redacted`], which
//!   formats as `[redacted]`. Redaction happens before formatting, so the
//!   value never reaches a buffer.
//! - Every event passes through [`BoundedLines`], which writes one line per
//!   event, escapes control characters so logged text cannot move the
//!   cursor or forge extra lines, and cuts lines at [`MAX_LINE_BYTES`].

use std::io::{self, Write};
use std::sync::Mutex;

use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;

/// Longest diagnostic line, including the shortened marker.
pub const MAX_LINE_BYTES: usize = 4 * 1024;

/// Name of the environment variable that enables diagnostics.
pub const LEVEL_VAR: &str = "HYAKUME_LOG";

const SHORTENED: &[u8] = b" [shortened]";

/// A value that must never appear in diagnostics.
pub struct Redacted<T>(pub T);

impl<T> std::fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

impl<T> std::fmt::Display for Redacted<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

/// Parses a `HYAKUME_LOG` value. Unknown values turn diagnostics off.
pub fn level(value: Option<&str>) -> LevelFilter {
    match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("error") => LevelFilter::ERROR,
        Some("warn") => LevelFilter::WARN,
        Some("info") => LevelFilter::INFO,
        Some("debug") => LevelFilter::DEBUG,
        Some("trace") => LevelFilter::TRACE,
        _ => LevelFilter::OFF,
    }
}

/// Installs the global diagnostics subscriber writing to standard error.
pub fn init_from_env() {
    let level = level(std::env::var(LEVEL_VAR).ok().as_deref());
    if level == LevelFilter::OFF {
        return;
    }
    // Ignore the error if a subscriber already exists.
    let _ = subscriber(level, BoundedLines::new(io::stderr())).try_init();
}

/// Builds the subscriber used by [`init_from_env`] around any sink.
pub fn subscriber<W>(
    level: LevelFilter,
    sink: BoundedLines<W>,
) -> tracing_subscriber::fmt::SubscriberBuilder<
    tracing_subscriber::fmt::format::DefaultFields,
    tracing_subscriber::fmt::format::Format,
    LevelFilter,
    BoundedLines<W>,
>
where
    W: Write + Send + 'static,
{
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_ansi(false)
        .with_target(false)
        .with_writer(sink)
}

/// A sink that writes each diagnostic event as one bounded, escaped line.
pub struct BoundedLines<W> {
    inner: Mutex<W>,
}

impl<W: Write> BoundedLines<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner: Mutex::new(inner),
        }
    }

    /// Returns the wrapped writer.
    pub fn into_inner(self) -> W {
        match self.inner.into_inner() {
            Ok(w) => w,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl<'a, W: Write + 'a> MakeWriter<'a> for BoundedLines<W> {
    type Writer = EventLine<'a, W>;

    fn make_writer(&'a self) -> Self::Writer {
        EventLine {
            sink: &self.inner,
            buf: Vec::new(),
        }
    }
}

/// Collects one event and writes it as a single line when dropped.
pub struct EventLine<'a, W: Write> {
    sink: &'a Mutex<W>,
    buf: Vec<u8>,
}

impl<W: Write> Write for EventLine<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Stop collecting well past the limit; the rest would be cut anyway.
        let room = (MAX_LINE_BYTES * 4).saturating_sub(self.buf.len());
        self.buf.extend_from_slice(&bytes[..bytes.len().min(room)]);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<W: Write> Drop for EventLine<'_, W> {
    fn drop(&mut self) {
        let line = bounded_line(&self.buf);
        let mut sink = match self.sink.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        // A failed diagnostic write must not disturb the program.
        let _ = sink.write_all(&line);
        let _ = sink.flush();
    }
}

/// Escapes control characters and bounds the result to one line of at most
/// [`MAX_LINE_BYTES`] bytes, ending in a newline.
fn bounded_line(raw: &[u8]) -> Vec<u8> {
    let body = raw.strip_suffix(b"\n").unwrap_or(raw);
    let limit = MAX_LINE_BYTES - SHORTENED.len() - 1;
    let mut out = Vec::with_capacity(body.len().min(MAX_LINE_BYTES));
    let mut shortened = false;
    for chunk in body.utf8_chunks() {
        let valid = chunk.valid().chars().map(|c| {
            if c.is_control() || is_hidden_format(c) {
                let code = u32::from(c);
                if code <= 0xff {
                    format!("\\x{code:02x}")
                } else {
                    format!("\\u{{{code:x}}}")
                }
            } else {
                c.to_string()
            }
        });
        let invalid = chunk.invalid().iter().map(|b| format!("\\x{b:02x}"));
        for piece in valid.chain(invalid) {
            if out.len() + piece.len() > limit {
                shortened = true;
                break;
            }
            out.extend_from_slice(piece.as_bytes());
        }
        if shortened {
            break;
        }
    }
    if shortened {
        out.extend_from_slice(SHORTENED);
    }
    out.push(b'\n');
    out
}

/// Characters that reorder or hide text without being control characters.
fn is_hidden_format(c: char) -> bool {
    matches!(
        c,
        '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{feff}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_or_missing_levels_turn_diagnostics_off() {
        assert_eq!(level(None), LevelFilter::OFF);
        assert_eq!(level(Some("verbose")), LevelFilter::OFF);
        assert_eq!(level(Some(" Debug ")), LevelFilter::DEBUG);
    }
}
