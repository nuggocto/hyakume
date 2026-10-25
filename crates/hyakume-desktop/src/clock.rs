//! The local time zone, for showing when comments were posted. The time
//! crate only reads local offsets while the process has a single thread, so
//! `sample` records them once at startup, daylight-saving changes included.

use time::{Duration, OffsetDateTime, UtcOffset};

/// How far back and ahead startup looks for offset changes. Older comments
/// use the earliest offset found, which is wrong only across a DST boundary.
const BACK: Duration = Duration::days(5 * 366);
const AHEAD: Duration = Duration::days(366);
/// Offsets are checked this often, then the exact change is searched for.
/// Two changes within one step would be missed; no zone does that.
const STEP: i64 = 7 * 24 * 3600;

pub struct LocalTime {
    /// Unix second from which each offset applies, oldest first.
    changes: Vec<(i64, UtcOffset)>,
}

impl LocalTime {
    /// Reads the local offsets around `now`. Call it before starting other
    /// threads; once they exist, every lookup falls back to UTC.
    pub fn sample(now: OffsetDateTime) -> Self {
        let at = |t: i64| {
            OffsetDateTime::from_unix_timestamp(t)
                .ok()
                .and_then(|moment| UtcOffset::local_offset_at(moment).ok())
                .unwrap_or(UtcOffset::UTC)
        };
        let (start, end) = (
            (now - BACK).unix_timestamp(),
            (now + AHEAD).unix_timestamp(),
        );
        let mut current = at(start);
        let mut changes = vec![(i64::MIN, current)];
        let mut t = start;
        while t < end {
            let next = t + STEP;
            let offset = at(next);
            if offset != current {
                // Binary search for the first second with the new offset.
                let (mut before, mut after) = (t, next);
                while after - before > 1 {
                    let mid = before + (after - before) / 2;
                    if at(mid) == current {
                        before = mid;
                    } else {
                        after = mid;
                    }
                }
                changes.push((after, offset));
                current = offset;
            }
            t = next;
        }
        Self { changes }
    }

    #[cfg(test)]
    pub fn from_changes(changes: Vec<(i64, UtcOffset)>) -> Self {
        Self { changes }
    }

    /// The local offset that applied at `moment`.
    pub fn offset_at(&self, moment: OffsetDateTime) -> UtcOffset {
        let t = moment.unix_timestamp();
        let after = self.changes.partition_point(|&(from, _)| from <= t);
        self.changes[after.saturating_sub(1)].1
    }
}
