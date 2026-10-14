//! Time as the core sees it. Domain code takes a [`Moment`] instead of
//! reading a clock, so tests choose exactly when things happen.
//!
//! Saved deadlines are stamped and checked with a [`LifetimeClock`], so
//! setting the wall clock back never changes how long work lives.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch on the wall clock, or 0 if the clock
/// reads earlier than the epoch.
pub fn wall_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Wall-clock milliseconds for checking lifetimes. It never reads earlier
/// than the latest time it gave, and between two readings it advances at
/// least as far as the monotonic clock did, so a wall clock set back does
/// not stop time for the lifetimes it checks.
#[derive(Clone, Copy, Debug)]
pub struct LifetimeClock {
    last: ClockReading,
    at: Instant,
}

/// One reading of a [`LifetimeClock`], saved so the next run can go on
/// from it: its time, and the wall clock's at that moment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClockReading {
    pub lifetime_ms: u64,
    pub wall_ms: u64,
}

impl LifetimeClock {
    /// Goes on from `saved`, the last reading an earlier run saved. The
    /// time while stopped is what the wall clock advanced since then. If
    /// the wall clock reads earlier than it did then, that time cannot be
    /// measured, and the second value is false.
    pub fn resume(saved: ClockReading, wall_ms: u64, mono: Instant) -> (Self, bool) {
        let stopped = wall_ms.saturating_sub(saved.wall_ms);
        let clock = Self {
            last: ClockReading {
                lifetime_ms: wall_ms.max(saved.lifetime_ms.saturating_add(stopped)),
                wall_ms,
            },
            at: mono,
        };
        (clock, wall_ms >= saved.wall_ms)
    }

    pub fn now(&mut self, wall_ms: u64, mono: Instant) -> u64 {
        let ran =
            u64::try_from(mono.saturating_duration_since(self.at).as_millis()).unwrap_or(u64::MAX);
        self.last = ClockReading {
            lifetime_ms: wall_ms.max(self.last.lifetime_ms.saturating_add(ran)),
            wall_ms,
        };

        // Only the whole milliseconds counted move on; the rest counts
        // toward the next reading, so frequent readings lose no time.
        self.at = self
            .at
            .checked_add(Duration::from_millis(ran))
            .unwrap_or(mono);
        self.last.lifetime_ms
    }

    /// The latest reading, for saving.
    pub fn reading(&self) -> ClockReading {
        self.last
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wall_clock_set_back_does_not_stop_time_for_lifetimes() {
        const DAY: u64 = 86_400_000;
        let start = Instant::now();
        let after = |days: u64| start + Duration::from_millis(days * DAY);
        let (mut clock, _) = LifetimeClock::resume(ClockReading::default(), 0, after(0));
        assert_eq!(clock.now(6 * DAY, after(6)), 6 * DAY);
        // The wall clock goes back three days, then two days pass.
        assert_eq!(clock.now(3 * DAY, after(6)), 6 * DAY);
        assert_eq!(clock.now(5 * DAY, after(8)), 8 * DAY);
        // A wall clock moved forward is believed.
        assert_eq!(clock.now(20 * DAY, after(8)), 20 * DAY);
    }

    #[test]
    fn readings_less_than_a_millisecond_apart_still_add_up() {
        let start = Instant::now();
        let (mut clock, _) = LifetimeClock::resume(ClockReading::default(), 10_000, start);
        // The wall clock went back, so only the monotonic clock counts.
        let mut now = 0;
        for n in 1..=4000 {
            now = clock.now(0, start + Duration::from_micros(500 * n));
        }
        assert_eq!(now, 12_000);
    }

    #[test]
    fn a_restart_counts_the_time_the_wall_clock_advanced_while_stopped() {
        const DAY: u64 = 86_400_000;
        let mono = Instant::now();
        // The last run saw the wall clock set back three days, at day six.
        let saved = ClockReading {
            lifetime_ms: 6 * DAY,
            wall_ms: 3 * DAY,
        };
        let (mut clock, measured) = LifetimeClock::resume(saved, 5 * DAY, mono);
        assert!(measured);
        assert_eq!(clock.now(5 * DAY, mono), 8 * DAY);
        // A wall clock behind the saved reading cannot measure the stop.
        let (_, measured) = LifetimeClock::resume(saved, 3 * DAY - 1, mono);
        assert!(!measured);
    }
}
