//! FILETIME values: the current time, and conversion to UTC and RFC 3339.
//!
//! A FILETIME counts 100 ns intervals since 1601-01-01 UTC.

use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, SecondsFormat, Utc};

/// FILETIME of the Unix epoch (1970-01-01T00:00:00Z).
pub(crate) const UNIX_EPOCH_FILETIME: u64 = 116_444_736_000_000_000;

/// 100 ns intervals per second.
const INTERVALS_PER_SECOND: u64 = 10_000_000;

/// Current system time as a FILETIME value.
pub(crate) fn now() -> u64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let intervals = u64::try_from(since_epoch.as_nanos() / 100).unwrap_or(u64::MAX);
    UNIX_EPOCH_FILETIME.saturating_add(intervals)
}

/// UTC time of a FILETIME; `None` for 0, for values before 1970 and for values beyond the
/// range chrono represents.
pub(crate) fn to_utc(filetime: u64) -> Option<DateTime<Utc>> {
    if filetime == 0 || filetime < UNIX_EPOCH_FILETIME {
        return None;
    }
    let since_epoch = filetime - UNIX_EPOCH_FILETIME;
    let seconds = i64::try_from(since_epoch / INTERVALS_PER_SECOND).ok()?;
    let nanos = u32::try_from((since_epoch % INTERVALS_PER_SECOND) * 100).ok()?;
    DateTime::from_timestamp(seconds, nanos)
}

/// RFC 3339 UTC text of a FILETIME in whole seconds (`2026-09-28T12:00:00Z`); `None` where
/// [`to_utc`] is `None`.
pub(crate) fn to_rfc3339(filetime: u64) -> Option<String> {
    to_utc(filetime).map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unix_epoch_converts_to_1970() {
        assert_eq!(
            to_rfc3339(UNIX_EPOCH_FILETIME).as_deref(),
            Some("1970-01-01T00:00:00Z")
        );
        assert_eq!(to_utc(UNIX_EPOCH_FILETIME), DateTime::from_timestamp(0, 0));
    }

    #[test]
    fn zero_and_pre_1970_values_have_no_time() {
        assert_eq!(to_utc(0), None);
        assert_eq!(to_utc(UNIX_EPOCH_FILETIME - 1), None);
        assert_eq!(to_utc(1), None);
        assert_eq!(to_rfc3339(0), None);
        // The largest FILETIME (year 60056) is still inside chrono's range.
        assert!(to_utc(u64::MAX).is_some());
    }

    #[test]
    fn fractions_are_kept_in_utc_and_dropped_in_text() {
        // 2026-10-04T12:00:00Z plus 1.5 s.
        let ft = UNIX_EPOCH_FILETIME + 1_791_115_200 * INTERVALS_PER_SECOND + 15_000_000;
        let utc = to_utc(ft).unwrap();
        assert_eq!(utc.timestamp(), 1_791_115_201);
        assert_eq!(utc.timestamp_subsec_nanos(), 500_000_000);
        assert_eq!(to_rfc3339(ft).as_deref(), Some("2026-10-04T12:00:01Z"));
    }

    #[test]
    fn now_is_after_the_epoch_and_advances() {
        let a = now();
        let b = now();
        assert!(a > UNIX_EPOCH_FILETIME);
        assert!(b >= a);
        let utc = to_utc(a).unwrap();
        let delta = (Utc::now() - utc).num_seconds().abs();
        assert!(delta < 60, "{delta}");
    }
}
