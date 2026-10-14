//! Diagnostics, off unless `HYAKUME_LOG` names a level. Secrets and payloads
//! go through [`Redacted`] before formatting, so they never reach a buffer.
//! [`BoundedLines`] escapes control and hidden characters and cuts lines at
//! [`MAX_LINE_BYTES`], so logged text cannot add lines or grow unbounded.

use std::fmt;
use std::io::{self, Write};
use std::sync::Mutex;

use tracing::level_filters::LevelFilter;
use tracing::{Event, Subscriber, span};
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::format::{DefaultFields, Format, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, FormattedFields, MakeWriter};
use tracing_subscriber::registry::LookupSpan;

use crate::display::is_hidden_format;

/// Longest diagnostic line, including the shortened marker.
pub const MAX_LINE_BYTES: usize = 4 * 1024;

/// Name of the environment variable that enables diagnostics.
pub const LEVEL_VAR: &str = "HYAKUME_LOG";

const SHORTENED: &[u8] = b" [shortened]";

/// A value diagnostics show only as `[redacted]`.
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
    // A second call, as in tests, keeps the subscriber already set.
    let _ = subscriber(level, BoundedLines::new(io::stderr())).try_init();
}

/// The subscriber [`init_from_env`] installs, around any sink so tests can
/// read what it writes.
pub fn subscriber<M>(
    level: LevelFilter,
    sink: M,
) -> tracing_subscriber::fmt::SubscriberBuilder<BoundedFields, BoundedFormat, LevelFilter, M>
where
    M: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let format = tracing_subscriber::fmt::format()
        .with_ansi(false)
        .with_target(false);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_ansi(false)
        .fmt_fields(BoundedFields)
        .event_format(BoundedFormat(format))
        .with_writer(sink)
}

/// The default event layout, keeping at most [`MAX_LINE_BYTES`] of each event.
pub struct BoundedFormat(Format);

impl<S, N> FormatEvent<S, N> for BoundedFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut capped = Capped::new(&mut writer, MAX_LINE_BYTES);
        self.0.format_event(ctx, Writer::new(&mut capped), event)
    }
}

/// The default field layout, keeping at most [`MAX_LINE_BYTES`] per span or event.
pub struct BoundedFields;

impl<'writer> FormatFields<'writer> for BoundedFields {
    fn format_fields<R: RecordFields>(
        &self,
        mut writer: Writer<'writer>,
        fields: R,
    ) -> fmt::Result {
        let mut capped = Capped::new(&mut writer, MAX_LINE_BYTES);
        DefaultFields::new().format_fields(Writer::new(&mut capped), fields)
    }

    fn add_fields(
        &self,
        current: &'writer mut FormattedFields<Self>,
        fields: &span::Record<'_>,
    ) -> fmt::Result {
        // Repeated records on one span must not grow its fields without end.
        if current.fields.len() >= MAX_LINE_BYTES {
            return Ok(());
        }
        if !current.fields.is_empty() {
            current.fields.push(' ');
        }
        self.format_fields(current.as_writer(), fields)
    }
}

/// Passes text through until `room` bytes are used, then drops the rest.
///
/// Dropping is not an error, so formatting finishes and the event is still
/// written. [`BoundedLines`] marks any line that reached the limit.
struct Capped<'a> {
    inner: &'a mut dyn fmt::Write,
    room: usize,
}

impl<'a> Capped<'a> {
    fn new(inner: &'a mut dyn fmt::Write, room: usize) -> Self {
        Self { inner, room }
    }
}

impl fmt::Write for Capped<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut end = text.len().min(self.room);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.room = if end < text.len() { 0 } else { self.room - end };
        self.inner.write_str(&text[..end])
    }
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
