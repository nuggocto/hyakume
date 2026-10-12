//! Time as the core sees it.
//!
//! Domain transitions take the current [`Moment`] as an argument instead of
//! reading a clock, so tests choose exactly when things happen. Runtime code
//! reads [`MonotonicClock`].

use std::time::{Duration, Instant};

/// Milliseconds since an arbitrary starting point on one endpoint's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Moment(u64);

impl Moment {
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }

    pub const fn as_millis(self) -> u64 {
        self.0
    }

    /// The moment `duration` later, stopping at the largest representable
    /// moment instead of wrapping.
    pub fn saturating_add(self, duration: Duration) -> Self {
        let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        Self(self.0.saturating_add(millis))
    }
}

/// Reads the operating system's monotonic clock as [`Moment`]s.
#[derive(Clone, Copy, Debug)]
pub struct MonotonicClock {
    start: Instant,
}

impl MonotonicClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    pub fn now(&self) -> Moment {
        let millis = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        Moment(millis)
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}
