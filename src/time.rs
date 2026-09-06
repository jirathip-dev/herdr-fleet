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

/// Convert an RFC3339 UTC timestamp (seconds precision, `Z` suffix — the
/// exact shape [`rfc3339_from_unix`] emits) back to Unix seconds. Returns
/// `None` for anything outside that fixed 20-char shape.
pub fn unix_from_rfc3339(text: &str) -> Option<i64> {
    if text.len() != 20 || !text.ends_with('Z') {
        return None;
    }
    let bytes = text.as_bytes();
    for (index, expected) in [(4usize, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')] {
        if bytes.get(index) != Some(&expected) {
            return None;
        }
    }
    let year: i64 = text[0..4].parse().ok()?;
    let month: i64 = text[5..7].parse().ok()?;
    let day: i64 = text[8..10].parse().ok()?;
    let hour: i64 = text[11..13].parse().ok()?;
    let minute: i64 = text[14..16].parse().ok()?;
    let second: i64 = text[17..19].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Days since 1970-01-01 via Howard Hinnant's days_from_civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400); // [0, 399]
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Current UTC time in RFC3339 seconds-`Z` form.
pub fn rfc3339_now() -> String {
    rfc3339_from_unix(unix_now())
}

/// Current Unix time in seconds since the epoch.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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

    #[test]
    fn unix_round_trips_through_rfc3339() {
        for unix in [0, 1_700_000_000, 1_752_902_388, 1_800_000_000, -1, -86_400] {
            let text = rfc3339_from_unix(unix);
            assert_eq!(
                unix_from_rfc3339(&text),
                Some(unix),
                "round trip failed for {unix}"
            );
        }
    }

    #[test]
    fn unix_rejects_malformed_or_out_of_range_shapes() {
        for text in [
            "2026-09-06T00:00:00",
            "2026-09-06 00:00:00Z",
            "2026-13-06T00:00:00Z",
            "2026-09-32T00:00:00Z",
            "2026-09-06T24:00:00Z",
            "not-a-time",
        ] {
            assert_eq!(unix_from_rfc3339(text), None, "shape {text:?} must refuse");
        }
    }
}
