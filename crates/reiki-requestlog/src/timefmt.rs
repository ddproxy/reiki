use std::time::{SystemTime, UNIX_EPOCH};

use reiki_plugin::format_utc;

/// `2026-10-23T21:04:10.860Z` for a point in time, without a date library.
pub fn rfc3339_millis(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs() as i64;
    let millis = since.subsec_millis();
    format_utc(secs, 'T', Some(millis), "Z")
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
