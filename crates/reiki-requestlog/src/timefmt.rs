use std::time::{SystemTime, UNIX_EPOCH};

/// `2026-10-23T21:04:10.860Z` for a point in time, without a date library.
pub fn rfc3339_millis(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs() as i64;
    let millis = since.subsec_millis();
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

// Days since 1970-01-01 to a proleptic Gregorian date (Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(secs: u64, millis: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(secs * 1000 + millis)
    }

    #[test]
    fn known_instants_format_correctly() {
        assert_eq!(rfc3339_millis(at(0, 0)), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            rfc3339_millis(at(951_782_400, 5)),
            "2000-02-29T00:00:00.005Z"
        );
        assert_eq!(
            rfc3339_millis(at(1_791_000_000, 860)),
            "2026-10-03T04:00:00.860Z"
        );
        assert_eq!(
            rfc3339_millis(at(4_102_444_799, 999)),
            "2099-12-31T23:59:59.999Z"
        );
    }

    #[test]
    fn leap_day_and_year_boundaries() {
        assert_eq!(
            rfc3339_millis(at(1_709_164_800, 0)),
            "2024-02-29T00:00:00.000Z"
        );
        assert_eq!(
            rfc3339_millis(at(1_709_251_200, 0)),
            "2024-03-01T00:00:00.000Z"
        );
        assert_eq!(
            rfc3339_millis(at(1_767_225_599, 0)),
            "2025-12-31T23:59:59.000Z"
        );
        assert_eq!(
            rfc3339_millis(at(1_767_225_600, 0)),
            "2026-01-01T00:00:00.000Z"
        );
    }
}
