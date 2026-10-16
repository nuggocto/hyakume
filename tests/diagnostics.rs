//! Each diagnostic event stays one bounded line, whatever text it carries,
//! so a peer's name or a huge value cannot forge or flood log lines.

// Test helpers may stop the test on unexpected I/O failure.
#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use hyakume::diagnostics::{BoundedLines, MAX_LINE_BYTES, subscriber};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;

/// A cloneable in-memory sink so the test can read what was written.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture(emit: impl FnOnce()) -> String {
    let sink = Captured::default();
    let dispatch = subscriber(LevelFilter::TRACE, BoundedLines::new(sink.clone())).finish();
    tracing::subscriber::with_default(dispatch, emit);
    let bytes = sink.0.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn peer_text_cannot_add_lines_or_terminal_controls() {
    let peer_name = "laptop\n2026-10-11 INFO accepted offer\x1b[2J\u{202e}";
    let out = capture(|| tracing::warn!(name = %peer_name, "refused a connection"));
    assert_eq!(out.lines().count(), 1, "{out:?}");
    assert!(
        !out.trim_end_matches('\n').chars().any(|c| c.is_control()),
        "{out:?}"
    );
    assert!(
        out.contains("\\x0a2026-10-11 INFO accepted offer\\x1b[2J\\u{202e}"),
        "{out:?}"
    );
}

#[test]
fn oversized_events_are_cut_and_marked() {
    let huge = "x".repeat(MAX_LINE_BYTES * 3);
    let out = capture(|| tracing::info!(value = %huge, "big"));
    assert!(out.len() <= MAX_LINE_BYTES, "{}", out.len());
    assert!(
        out.ends_with(" [shortened]\n"),
        "{}",
        &out[out.len() - 40..]
    );
}

/// Records the size of each formatted event before any sink limit applies.
#[derive(Clone, Default)]
struct EventSizes(Arc<Mutex<Vec<usize>>>);

impl std::io::Write for EventSizes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().push(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for EventSizes {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

#[test]
fn huge_fields_are_not_formatted_past_the_line_limit() {
    let sizes = EventSizes::default();
    let dispatch = subscriber(LevelFilter::TRACE, sizes.clone()).finish();
    let huge = "x".repeat(MAX_LINE_BYTES * 64);
    tracing::subscriber::with_default(dispatch, || {
        tracing::info!(value = %huge, debug = ?huge, "big");
    });
    let sizes = sizes.0.lock().unwrap().clone();
    assert_eq!(sizes.len(), 1, "{sizes:?}");
    assert!(sizes[0] <= MAX_LINE_BYTES, "{sizes:?}");
}
