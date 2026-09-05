//! UTC time helpers for the RFC3339 (seconds, `Z`) timestamps the #3
//! observation/output families require.

use std::time::{SystemTime, UNIX_EPOCH};

/// Format a Unix timestamp (seconds since the epoch) as RFC3339 UTC with
/// seconds precision and a `Z` suffix, e.g. `2026-09-06T00:00:00Z`.
pub fn rfc3339_from_unix(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Current UTC time in RFC3339 seconds-`Z` form.
pub fn rfc3339_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    rfc3339_from_unix(seconds)
}

/// Convert days since 1970-01-01 to a (year, month, day) civil date using
/// Howard Hinnant's `civil_from_days` algorithm (valid for a wide year
/// range; tests cover the epoch and modern dates).
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_1970_01_01() {
        assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn known_modern_instants() {
        assert_eq!(rfc3339_from_unix(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339_from_unix(1_752_902_388), "2025-07-19T05:19:48Z");
        assert_eq!(rfc3339_from_unix(1_800_000_000), "2027-01-15T08:00:00Z");
    }

    #[test]
    fn handles_negative_instants() {
        assert_eq!(rfc3339_from_unix(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn seconds_precision_shape() {
        let text = rfc3339_now();
        assert_eq!(text.len(), 20, "YYYY-MM-DDTHH:MM:SSZ is 20 chars");
        assert!(text.ends_with('Z'));
    }
}
