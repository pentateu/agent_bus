//! Clock helpers for timestamps on the wire and in the journal.
//!
//! The core crate stays otherwise pure; this module is the one permitted
//! exception (mirroring `agent-bus-core`), reading the system clock and
//! nothing else.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Process-global monotonic ULID source (see agent-bus-core's note: a pure
/// clock-derived `Ulid` can tie within a millisecond; the shared
/// [`ulid::Generator`] bumps the random component instead of colliding).
static GENERATOR: Mutex<ulid::Generator> = Mutex::new(ulid::Generator::new());

/// The next monotonic ULID.
#[must_use]
pub fn new_ulid() -> ulid::Ulid {
    let mut guard = GENERATOR.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.generate().unwrap_or_else(ulid::Overflow::commit_overflow_increment)
}

/// Current time as an RFC 3339 / ISO 8601 UTC string with millisecond precision.
#[must_use]
pub fn now_rfc3339() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let millis = d.subsec_millis();

    let days = secs / 86_400;
    let seconds_of_day = secs % 86_400;
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    let (year, month, day) = civil_from_days(i64::try_from(days).unwrap_or(0));

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Convert days since the Unix epoch to a civil (year, month, day).
///
/// Howard Hinnant's `civil_from_days` algorithm, kept in `i64` throughout so
/// that no lossy cast is needed.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, u32::try_from(m).unwrap_or(0), u32::try_from(d).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_shape_is_stable() {
        let ts = now_rfc3339();
        let (date, time) = ts.split_once('T').expect("a T separates date and time");
        assert_eq!(date.len(), 10, "YYYY-MM-DD");
        assert_eq!(time.len(), 13, "HH:MM:SS.mmmZ");
        assert!(time.ends_with('Z'));
    }

    #[test]
    fn timestamps_are_ordered() {
        let a = now_rfc3339();
        let b = now_rfc3339();
        assert!(a <= b);
    }
}
