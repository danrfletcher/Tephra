//! `tephractl fixes --since <30m|2h|7d|ISO date>` (G18): what the value
//! means, decided with no network. `fix.list` takes epoch seconds, so
//! the value is turned into those at the moment the command runs.
//!
//! - `30m`, `2h`, `7d`: that long ago (whole numbers of minutes, hours or
//!   days, 1 or more).
//! - `2026-11-16`, `2026-11-16T09:30`, `2026-11-16T09:30:15` (a space may
//!   replace the `T`): a moment in the machine's local zone, the same
//!   zone the command prints times in. A date alone is local midnight.
//! - Any of those ISO forms with a trailing `Z` or `+hh:mm` / `-hh:mm`
//!   is read in that zone instead.
//!
//! A value from before 1970 is refused.

use std::fmt;

/// A parsed `--since`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Since {
    /// This many seconds before now.
    Ago(i64),
    /// This moment, in epoch seconds.
    At(i64),
}

/// Why a `--since` value was refused. The text names the accepted forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinceError(pub String);

impl fmt::Display for SinceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn refuse(value: &str, why: &str) -> SinceError {
    SinceError(format!(
        "invalid --since value {value:?}: {why} (use 30m, 2h, 7d or a date such as 2026-11-16 or 2026-11-16T09:30)"
    ))
}

impl Since {
    pub fn parse(value: &str) -> Result<Since, SinceError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(refuse(value, "it is empty"));
        }
        if let Some(ago) = parse_ago(value)? {
            return Ok(Since::Ago(ago));
        }
        parse_iso(value).map(Since::At)
    }

    /// The epoch second `fix.list` is asked for, given the current time.
    /// An "ago" earlier than 1970 is 0.
    pub fn epoch(self, now: i64) -> i64 {
        match self {
            Since::Ago(secs) => now.saturating_sub(secs).max(0),
            Since::At(at) => at,
        }
    }
}

/// `Some(seconds)` for `30m`, `2h` or `7d`; `None` when the value does
/// not end in one of those units after digits (so it may be a date).
fn parse_ago(value: &str) -> Result<Option<i64>, SinceError> {
    let Some(unit) = value.chars().last() else {
        return Ok(None);
    };
    let per_unit: i64 = match unit {
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        _ => return Ok(None),
    };
    let digits = &value[..value.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(None);
    }
    let count: i64 = digits
        .parse()
        .map_err(|_| refuse(value, "the number is too large"))?;
    if count == 0 {
        return Err(refuse(value, "the amount must be 1 or more"));
    }
    count
        .checked_mul(per_unit)
        .ok_or_else(|| refuse(value, "the number is too large"))
        .map(Some)
}

fn parse_iso(value: &str) -> Result<i64, SinceError> {
    let bad = |why: &str| refuse(value, why);

    // Split off a zone: `Z`, `+hh:mm` or `-hh:mm` after the time. A date
    // alone has `-` only inside the date, which is 10 characters.
    let (body, zone_secs) = if let Some(body) = value.strip_suffix('Z') {
        (body, Some(0i64))
    } else if value.len() > 10 {
        // Byte 10 may fall inside a multi-byte character; no valid date
        // has one, so refuse instead of slicing there.
        let tail = value
            .get(10..)
            .ok_or_else(|| bad("it is not a duration or a date"))?;
        match tail.rfind(['+', '-']).map(|i| i + 10) {
            Some(at) => {
                let sign = if value.as_bytes()[at] == b'+' { 1 } else { -1 };
                let zone = &value[at + 1..];
                let (hh, mm) = zone
                    .split_once(':')
                    .ok_or_else(|| bad("a zone offset is +hh:mm or -hh:mm"))?;
                let hh = number(hh, 2).ok_or_else(|| bad("a zone offset is +hh:mm or -hh:mm"))?;
                let mm = number(mm, 2).ok_or_else(|| bad("a zone offset is +hh:mm or -hh:mm"))?;
                if hh > 23 || mm > 59 {
                    return Err(bad("the zone offset is out of range"));
                }
                (&value[..at], Some(sign * (hh * 3_600 + mm * 60)))
            }
            None => (value, None),
        }
    } else {
        (value, None)
    };

    let (date, time) = match body.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (body, None),
    };
    let mut parts = date.split('-');
    let (Some(y), Some(mo), Some(d), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad("it is not a duration or a date"));
    };
    let year = number(y, 4).ok_or_else(|| bad("the year is 4 digits"))?;
    let month = number(mo, 2).ok_or_else(|| bad("the month is 2 digits"))?;
    let day = number(d, 2).ok_or_else(|| bad("the day is 2 digits"))?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return Err(bad("there is no such date"));
    }
    let (hour, minute, second) = match time {
        None => (0, 0, 0),
        Some(t) => {
            let mut p = t.split(':');
            let h = p.next().and_then(|s| number(s, 2));
            let m = p.next().and_then(|s| number(s, 2));
            let s = match p.next() {
                None => Some(0),
                Some(s) => number(s, 2),
            };
            if p.next().is_some() {
                return Err(bad("the time is hh:mm or hh:mm:ss"));
            }
            match (h, m, s) {
                (Some(h), Some(m), Some(s)) if h <= 23 && m <= 59 && s <= 59 => (h, m, s),
                _ => return Err(bad("the time is hh:mm or hh:mm:ss")),
            }
        }
    };

    let epoch = match zone_secs {
        Some(offset) => {
            days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
                - offset
        }
        None => local_epoch(year, month, day, hour, minute, second)
            .ok_or_else(|| bad("that local time cannot be converted"))?,
    };
    if epoch < 0 {
        return Err(bad("it is before 1970"));
    }
    Ok(epoch)
}

/// `text` as a number when it is exactly `digits` ASCII digits.
fn number(text: &str, digits: usize) -> Option<i64> {
    if text.len() == digits && text.bytes().all(|b| b.is_ascii_digit()) {
        text.parse().ok()
    } else {
        None
    }
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to the given civil date (proleptic Gregorian).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The epoch second of a wall-clock time in the machine's local zone;
/// `None` when the C library cannot convert it.
fn local_epoch(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
) -> Option<i64> {
    // SAFETY: `tm` is plain data; `mktime` reads and normalizes it and
    // touches nothing else. `tm_isdst = -1` lets the library decide.
    let t = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_year = (year - 1900) as libc::c_int;
        tm.tm_mon = (month - 1) as libc::c_int;
        tm.tm_mday = day as libc::c_int;
        tm.tm_hour = hour as libc::c_int;
        tm.tm_min = minute as libc::c_int;
        tm.tm_sec = second as libc::c_int;
        tm.tm_isdst = -1;
        libc::mktime(&mut tm)
    };
    if t == -1 {
        None
    } else {
        Some(t as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_minutes_hours_and_days_ago() {
        assert_eq!(Since::parse("30m"), Ok(Since::Ago(1_800)));
        assert_eq!(Since::parse("2h"), Ok(Since::Ago(7_200)));
        assert_eq!(Since::parse("7d"), Ok(Since::Ago(604_800)));
        assert_eq!(Since::Ago(1_800).epoch(10_000), 8_200);
        assert_eq!(Since::Ago(1_800).epoch(100), 0, "never before 1970");
    }

    #[test]
    fn zoned_dates_are_exact() {
        // 2026-11-16T00:00:00Z
        assert_eq!(
            Since::parse("2026-11-16T00:00:00Z"),
            Ok(Since::At(1_794_787_200))
        );
        assert_eq!(
            Since::parse("2026-11-16T09:30Z"),
            Ok(Since::At(1_794_787_200 + 34_200))
        );
        assert_eq!(
            Since::parse("2026-11-16T09:30:15+01:00"),
            Ok(Since::At(1_794_787_200 + 34_215 - 3_600))
        );
        assert_eq!(Since::parse("1970-01-01T00:00:00Z"), Ok(Since::At(0)));
        assert_eq!(
            Since::parse("2024-02-29T00:00Z"),
            Ok(Since::At(1_709_164_800))
        );
    }

    #[test]
    fn refuses_what_is_not_a_duration_or_a_date() {
        for bad in [
            "",
            "m",
            "0m",
            "-5m",
            "1.5h",
            "5x",
            "5",
            "30 m",
            "99999999999999999999d",
            "2026-13-01",
            "2026-02-30",
            "2025-02-29",
            "2026-11-16T25:00",
            "2026-11-16T09",
            "2026-11-16T09:61",
            "26-11-16",
            "2026/11/16",
            "yesterday",
            "1969-12-31T23:59:59Z",
            "2026-11-16T09:30+25:00",
            "2026-11-16T09:30+0100",
            // Multi-byte character across byte 10 (used to panic).
            "2026-11-1é5",
            "2026-11-1é",
            "2026-11-1\u{1F600}T09:30",
            "é2026-11-16",
        ] {
            let err = Since::parse(bad).expect_err(bad);
            assert!(err.0.contains("--since"), "{bad}: {err}");
        }
    }

    #[test]
    fn local_dates_parse_to_a_moment_within_a_day_of_utc() {
        let Ok(Since::At(local)) = Since::parse("2026-11-16") else {
            panic!("a date alone is local midnight");
        };
        assert!((local - 1_794_787_200).abs() <= 14 * 3_600);
        assert_eq!(Since::parse("2026-11-16 00:00"), Since::parse("2026-11-16"));
        assert_eq!(
            Since::parse("2026-11-16T00:00:00"),
            Since::parse("2026-11-16")
        );
    }
}
