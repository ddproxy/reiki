use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::classify::{civil_from_days, days_from_civil};

const SECONDS_PER_DAY: i64 = 86_400;
const MAX_OFFSET_SECONDS: i64 = 14 * 3_600;
/// A computed wait longer than this is rejected so a misconfigured schedule can
/// never park a key for an unbounded time.
const MAX_WAIT: Duration = Duration::from_secs(31 * SECONDS_PER_DAY as u64);

/// A recurring quota reset instant for one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reset {
    Daily {
        hour: u32,
        minute: u32,
        offset_seconds: i64,
    },
    Monthly {
        day: u32,
        hour: u32,
        minute: u32,
        offset_seconds: i64,
    },
}

impl Reset {
    /// Parse `daily HH:MM <zone>` or `monthly D HH:MM <zone>`, where `<zone>` is
    /// `UTC` or a fixed `+HH:MM`/`-HH:MM` offset.
    pub fn parse(value: &str) -> Result<Self, String> {
        let parts: Vec<&str> = value.split_whitespace().collect();
        match parts.as_slice() {
            ["daily", time, zone] => {
                let (hour, minute) = parse_clock(time)?;
                let offset_seconds = parse_zone(zone)?;
                Ok(Reset::Daily {
                    hour,
                    minute,
                    offset_seconds,
                })
            }
            ["monthly", day, time, zone] => {
                let day = parse_day_of_month(day)?;
                let (hour, minute) = parse_clock(time)?;
                let offset_seconds = parse_zone(zone)?;
                Ok(Reset::Monthly {
                    day,
                    hour,
                    minute,
                    offset_seconds,
                })
            }
            _ => Err(format!(
                "`{value}` is not `daily HH:MM <zone>` or `monthly D HH:MM <zone>`"
            )),
        }
    }

    /// Wait until the first reset instant strictly after `now`. `None` when the
    /// wait is longer than the fallback window, so callers keep the default
    /// cooldown.
    pub fn next_after(self, now: SystemTime) -> Option<Duration> {
        let now_seconds = now.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
        let wait = match self {
            Reset::Daily {
                hour,
                minute,
                offset_seconds,
            } => next_daily(now_seconds, hour, minute, offset_seconds),
            Reset::Monthly {
                day,
                hour,
                minute,
                offset_seconds,
            } => next_monthly(now_seconds, day, hour, minute, offset_seconds),
        };
        (wait > 0 && wait <= MAX_WAIT.as_secs() as i64).then(|| Duration::from_secs(wait as u64))
    }
}

fn next_daily(now_seconds: i64, hour: u32, minute: u32, offset_seconds: i64) -> i64 {
    let local = now_seconds + offset_seconds;
    let today = local.div_euclid(SECONDS_PER_DAY);
    let second_of_day = local.rem_euclid(SECONDS_PER_DAY);
    let target_second = (hour * 3_600 + minute * 60) as i64;
    let target_day = if target_second > second_of_day {
        today
    } else {
        today + 1
    };
    target_day * SECONDS_PER_DAY + target_second - offset_seconds - now_seconds
}

fn next_monthly(
    now_seconds: i64,
    day_of_month: u32,
    hour: u32,
    minute: u32,
    offset_seconds: i64,
) -> i64 {
    let local = now_seconds + offset_seconds;
    let (year, month, _) = civil_from_days(local.div_euclid(SECONDS_PER_DAY));
    let target_second = (hour * 3_600 + minute * 60) as i64;
    let day = day_of_month as i64;
    let mut candidate = days_from_civil(year, month, day) * SECONDS_PER_DAY + target_second;
    if candidate <= local {
        let (year, month) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
        candidate = days_from_civil(year, month, day) * SECONDS_PER_DAY + target_second;
    }
    candidate - offset_seconds - now_seconds
}

fn parse_clock(value: &str) -> Result<(u32, u32), String> {
    let (hour, minute) = value
        .split_once(':')
        .ok_or_else(|| format!("time `{value}` is not HH:MM"))?;
    let hour = parse_uint(hour, 2).ok_or_else(|| format!("time `{value}` has an invalid hour"))?;
    let minute =
        parse_uint(minute, 2).ok_or_else(|| format!("time `{value}` has an invalid minute"))?;
    if hour > 23 || minute > 59 {
        return Err(format!("time `{value}` is out of range"));
    }
    Ok((hour, minute))
}

fn parse_day_of_month(value: &str) -> Result<u32, String> {
    let day: u32 = value
        .parse()
        .map_err(|_| format!("day `{value}` is not a number"))?;
    if !(1..=28).contains(&day) {
        return Err(format!("day `{value}` must be between 1 and 28"));
    }
    Ok(day)
}

fn parse_zone(value: &str) -> Result<i64, String> {
    if value.eq_ignore_ascii_case("UTC") {
        return Ok(0);
    }
    let (sign, rest) = match value.strip_prefix('+') {
        Some(rest) => (1, rest),
        None => match value.strip_prefix('-') {
            Some(rest) => (-1, rest),
            None => return Err(format!("offset `{value}` must be +HH:MM, -HH:MM, or UTC")),
        },
    };
    let (hour, minute) = rest
        .split_once(':')
        .ok_or_else(|| format!("offset `{value}` is not HH:MM"))?;
    let hour =
        parse_uint(hour, 2).ok_or_else(|| format!("offset `{value}` has an invalid hour"))?;
    let minute =
        parse_uint(minute, 2).ok_or_else(|| format!("offset `{value}` has an invalid minute"))?;
    if minute > 59 {
        return Err(format!("offset `{value}` is out of range"));
    }
    let offset = sign * (hour as i64 * 3_600 + minute as i64 * 60);
    if offset.abs() > MAX_OFFSET_SECONDS {
        return Err(format!("offset `{value}` is out of range"));
    }
    Ok(offset)
}

fn parse_uint(value: &str, width: usize) -> Option<u32> {
    if value.len() != width || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(year: i64, month: i64, day: i64, hour: i64, minute: i64, second: i64) -> SystemTime {
        let seconds =
            days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
        UNIX_EPOCH + Duration::from_secs(seconds as u64)
    }

    fn parse(value: &str) -> Reset {
        Reset::parse(value).unwrap()
    }

    fn wait(value: &str, now: SystemTime) -> Option<u64> {
        parse(value).next_after(now).map(|d| d.as_secs())
    }

    #[test]
    fn parses_the_daily_form() {
        assert_eq!(
            parse("daily 00:00 UTC"),
            Reset::Daily {
                hour: 0,
                minute: 0,
                offset_seconds: 0
            }
        );
        assert_eq!(
            parse("daily 03:30 +02:00"),
            Reset::Daily {
                hour: 3,
                minute: 30,
                offset_seconds: 7_200
            }
        );
        assert_eq!(
            parse("daily 23:59 -14:00"),
            Reset::Daily {
                hour: 23,
                minute: 59,
                offset_seconds: -50_400
            }
        );
        assert_eq!(
            parse("daily 12:00 utc"),
            Reset::Daily {
                hour: 12,
                minute: 0,
                offset_seconds: 0
            }
        );
    }

    #[test]
    fn parses_the_monthly_form() {
        assert_eq!(
            parse("monthly 1 00:00 UTC"),
            Reset::Monthly {
                day: 1,
                hour: 0,
                minute: 0,
                offset_seconds: 0
            }
        );
        assert_eq!(
            parse("monthly 28 23:59 +05:30"),
            Reset::Monthly {
                day: 28,
                hour: 23,
                minute: 59,
                offset_seconds: 19_800
            }
        );
    }

    #[test]
    fn rejects_unknown_or_malformed_schedules() {
        for value in [
            "",
            "daily",
            "daily 00:00",
            "weekly 00:00 UTC",
            "daily 00:00 CET",
            "daily 00:00 Z",
            "daily 24:00 UTC",
            "daily 00:60 UTC",
            "daily 0:00 UTC",
            "daily 000:00 UTC",
            "daily -1:00 UTC",
            "daily 00:00 +15:00",
            "daily 00:00 -15:00",
            "daily 00:00 02:00",
            "daily 00:00 +2:00",
            "daily 00:00 +02",
            "monthly 0 00:00 UTC",
            "monthly 29 00:00 UTC",
            "monthly 1 00:00",
            "monthly one 00:00 UTC",
            "monthly 1 24:00 UTC",
        ] {
            assert!(Reset::parse(value).is_err(), "`{value}` should be rejected");
        }
    }

    #[test]
    fn daily_reset_just_before_exactly_at_and_just_after() {
        let reset = "daily 00:00 UTC";
        assert_eq!(
            wait(reset, utc(2026, 1, 1, 23, 59, 59)),
            Some(1),
            "just before the reset"
        );
        assert_eq!(
            wait(reset, utc(2026, 1, 2, 0, 0, 0)),
            Some(86_400),
            "exactly at the reset rolls to the next day"
        );
        assert_eq!(
            wait(reset, utc(2026, 1, 2, 0, 0, 1)),
            Some(86_399),
            "just after the reset"
        );
    }

    #[test]
    fn daily_reset_uses_the_fixed_offset() {
        // 00:30 +02:00 is 22:30 UTC the previous day.
        let reset = "daily 00:30 +02:00";
        assert_eq!(
            wait(reset, utc(2026, 1, 1, 22, 0, 0)),
            Some(1_800),
            "in the window before the local reset"
        );
        assert_eq!(
            wait(reset, utc(2026, 1, 1, 22, 30, 0)),
            Some(86_400),
            "exactly at the local reset"
        );
        assert_eq!(
            wait(reset, utc(2026, 1, 1, 23, 0, 0)),
            Some(84_600),
            "24 h minus 30 min"
        );
    }

    #[test]
    fn monthly_reset_crosses_month_boundaries() {
        let reset = "monthly 1 00:00 UTC";
        assert_eq!(
            wait(reset, utc(2026, 1, 31, 12, 0, 0)),
            Some(43_200),
            "end of a 31-day month"
        );
        assert_eq!(
            wait(reset, utc(2026, 4, 30, 23, 0, 0)),
            Some(3_600),
            "end of a 30-day month"
        );
    }

    #[test]
    fn monthly_reset_crosses_the_year_boundary() {
        assert_eq!(
            wait("monthly 1 00:00 UTC", utc(2026, 12, 15, 0, 0, 0)),
            Some(17 * 86_400),
        );
    }

    #[test]
    fn monthly_reset_handles_leap_years() {
        let reset = "monthly 1 00:00 UTC";
        assert_eq!(
            wait(reset, utc(2024, 2, 28, 12, 0, 0)),
            Some(36 * 3_600),
            "leap year: 29 Feb exists"
        );
        assert_eq!(
            wait(reset, utc(2023, 2, 28, 12, 0, 0)),
            Some(12 * 3_600),
            "common year: 1 Mar follows 28 Feb"
        );
        assert_eq!(
            wait(reset, utc(2024, 2, 29, 12, 0, 0)),
            Some(12 * 3_600),
            "leap day itself"
        );
    }

    #[test]
    fn a_time_before_the_epoch_yields_no_wait() {
        let before_epoch = UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(parse("daily 00:00 UTC").next_after(before_epoch), None);
    }
}
