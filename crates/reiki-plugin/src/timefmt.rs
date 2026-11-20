//! Date and time formatting shared by the host and the request log plugin.

/// Days since 1970-01-01 to a proleptic Gregorian `(year, month, day)`.
///
/// Howard Hinnant's `civil_from_days`; the inverse of the host's `days_from_civil`.
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
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

/// Unix `secs` as `YYYY-MM-DD<separator>HH:MM:SS[.mmm]<suffix>` in UTC, without a date library.
///
/// `millis` adds a three-digit fraction and `suffix` is appended verbatim, so callers can ask for
/// an RFC 3339 stamp (`'T'`, `Some(ms)`, `"Z"`), a plain UTC log time (`' '`, `None`, `""`) or
/// whole seconds (`'T'`, `None`, `"Z"`).
pub fn format_utc(secs: i64, separator: char, millis: Option<u32>, suffix: &str) -> String {
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3_600, rem % 3_600 / 60, rem % 60);
    match millis {
        Some(millis) => format!(
            "{year:04}-{month:02}-{day:02}{separator}{hour:02}:{minute:02}:{second:02}.{millis:03}{suffix}"
        ),
        None => format!(
            "{year:04}-{month:02}-{day:02}{separator}{hour:02}:{minute:02}:{second:02}{suffix}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_knows_epoch_leap_days_and_dates_before_it() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(1_709_164_800 / 86_400), (2024, 2, 29));
        assert_eq!(civil_from_days(11_687), (2001, 12, 31));
    }

    #[test]
    fn format_utc_keeps_each_caller_shape() {
        assert_eq!(format_utc(0, 'T', None, "Z"), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_utc(1_791_000_000, 'T', Some(860), "Z"),
            "2026-10-03T04:00:00.860Z"
        );
        assert_eq!(
            format_utc(1_791_000_000, ' ', None, ""),
            "2026-10-03 04:00:00"
        );
        assert_eq!(format_utc(-1, 'T', None, "Z"), "1969-12-31T23:59:59Z");
    }
}
