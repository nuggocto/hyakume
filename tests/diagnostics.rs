//! Diagnostics keep secrets out and stay one bounded line per event.

// Test helpers may stop the test on unexpected I/O failure.
#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use hyakume::diagnostics::{BoundedLines, MAX_LINE_BYTES, Redacted, subscriber};
use tracing::level_filters::LevelFilter;

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
fn redacted_secrets_and_payloads_never_reach_the_output() {
    let key = "k3y-6b1f0e8d2c4a";
    let payload = "remember the milk";
    let out = capture(|| {
        tracing::info!(key = %Redacted(key), payload = ?Redacted(payload.as_bytes()), "pairing step");
    });
    assert!(out.contains("pairing step"), "{out}");
    assert!(out.contains("[redacted]"), "{out}");
    assert!(!out.contains(key), "{out}");
    assert!(!out.contains(payload), "{out}");
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
