//! Time helpers on plain `std`: milliseconds since the Unix epoch, ISO-8601
//! formatting in UTC, durations like `90m`.

use crate::error::{AitError, Result};
use std::time::{SystemTime, UNIX_EPOCH};

/// Current time in ms. `AIT_NOW_MS` overrides the clock (used by tests to
/// simulate leases expiring without sleeping).
pub fn now_ms() -> Result<i64> {
    if let Ok(v) = std::env::var("AIT_NOW_MS")
        && !v.trim().is_empty()
    {
        return v
            .trim()
            .parse::<i64>()
            .map_err(|_| AitError::usage("AIT_NOW_MS must be milliseconds since 1970"));
    }
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AitError::new("CLOCK", "system clock is before 1970"))?;
    Ok(i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

pub fn iso_opt(ms: Option<i64>) -> serde_json::Value {
    match ms {
        Some(v) => serde_json::Value::String(iso(v)),
        None => serde_json::Value::Null,
    }
}

/// `YYYY-MM-DD` for the UTC day containing `ms`.
pub fn day(ms: i64) -> String {
    iso(ms)[..10].to_string()
}

/// Days since 1970-01-01 for the UTC day containing `ms`.
pub fn day_number(ms: i64) -> i64 {
    ms.div_euclid(86_400_000)
}

/// Parses `YYYY-MM-DD` into a day number.
pub fn parse_date(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let m: i64 = s.get(5..7)?.parse().ok()?;
    let d: i64 = s.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let n = days_from_civil(y, m, d);
    // Reject impossible dates such as 2026-02-31.
    if civil_from_days(n) != (y, m, d) {
        return None;
    }
    Some(n)
}

/// Parses `45s`, `30m`, `2h`, `7d`, `1w` (a bare number means minutes).
pub fn parse_duration(s: &str) -> Result<i64> {
    let s = s.trim();
    let bad = || AitError::usage(format!("bad duration '{s}' (use e.g. 30m, 2h, 7d)"));
    let (num, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => (&s[..i], &s[i..]),
        None => (s, "m"),
    };
    let n: i64 = num.parse().map_err(|_| bad())?;
    let mult = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 604_800_000,
        _ => return Err(bad()),
    };
    n.checked_mul(mult).ok_or_else(bad)
}

/// Short human description of `ms` relative to `now`: `5m ago`, `in 2h`.
pub fn rel(now: i64, ms: i64) -> String {
    let diff = ms - now;
    let a = diff.abs() / 1000;
    let s = if a < 60 {
        format!("{a}s")
    } else if a < 3600 {
        format!("{}m", a / 60)
    } else if a < 86_400 {
        format!("{}h{:02}m", a / 3600, a % 3600 / 60)
    } else {
        format!("{}d", a / 86_400)
    };
    if diff >= 0 {
        format!("in {s}")
    } else {
        format!("{s} ago")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_formats_known_instants() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_764_135_220_000), "2025-11-26T05:33:40Z");
        assert_eq!(iso(951_782_400_000), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn dates_round_trip() {
        let n = parse_date("2026-11-25").unwrap();
        assert_eq!(day(n * 86_400_000), "2026-11-25");
        assert!(parse_date("2026-02-31").is_none());
        assert!(parse_date("2026-1-01").is_none());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90s").unwrap(), 90_000);
        assert_eq!(parse_duration("1h").unwrap(), 3_600_000);
        assert_eq!(parse_duration("15").unwrap(), 900_000);
        assert!(parse_duration("1y").is_err());
        assert!(parse_duration("h").is_err());
    }

    #[test]
    fn relative() {
        assert_eq!(rel(100_000, 40_000), "1m ago");
        assert_eq!(rel(0, 7_200_000), "in 2h00m");
    }
}
