use std::str::FromStr;

use time::{Date, OffsetDateTime, format_description::well_known::Iso8601};

use crate::StoreError;

pub fn now_ms() -> i64 {
    millis(OffsetDateTime::now_utc())
}

pub fn millis(t: OffsetDateTime) -> i64 {
    (t.unix_timestamp_nanos() / 1_000_000) as i64
}

pub fn from_millis(ms: i64) -> Result<OffsetDateTime, StoreError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .map_err(|e| StoreError::Corrupt(format!("timestamp {ms}: {e}")))
}

/// Parses an id column, treating a malformed value as corrupt storage.
pub fn id<T: FromStr>(s: &str) -> Result<T, StoreError> {
    s.parse()
        .map_err(|_| StoreError::Corrupt(format!("malformed id {s:?}")))
}

pub fn opt_id<T: FromStr>(s: Option<&str>) -> Result<Option<T>, StoreError> {
    s.map(id).transpose()
}

pub fn date_to_text(d: Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

pub fn date_from_text(s: &str) -> Result<Date, StoreError> {
    Date::parse(s, &Iso8601::DATE).map_err(|e| StoreError::Corrupt(format!("date {s:?}: {e}")))
}
