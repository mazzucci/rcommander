//! Display helpers shared by the front ends.

/// `1023` → `"1023"`, `1536` → `"1.5K"`, `5_000_000` → `"4.8M"`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["K", "M", "G", "T", "P"];
    if bytes < 1024 {
        return bytes.to_string();
    }
    let mut v = bytes as f64;
    let mut unit = "";
    for u in UNITS {
        v /= 1024.0;
        unit = u;
        if v < 1024.0 {
            break;
        }
    }
    // Decide on decimals after rounding, so 9.99K prints as "10K", not "10.0K".
    if (v * 10.0).round() < 100.0 {
        format!("{v:.1}{unit}")
    } else {
        format!("{v:.0}{unit}")
    }
}

/// Unix seconds plus a UTC offset → `"2026-10-14 13:05"`.
pub fn format_time(secs: i64, utc_offset_secs: i64) -> String {
    let t = secs + utc_offset_secs;
    let (days, rem) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(human_size(999), "999");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(5_000_000), "4.8M");
        assert_eq!(human_size(1024), "1.0K");
        assert_eq!(human_size(10239), "10K");
    }

    #[test]
    fn times() {
        assert_eq!(format_time(0, 0), "1970-01-01 00:00");
        assert_eq!(format_time(1_791_979_500, 0), "2026-10-14 12:05");
        assert_eq!(format_time(1_791_979_500, 3600), "2026-10-14 13:05");
        assert_eq!(format_time(1_791_979_500, -13 * 3600), "2026-10-13 23:05");
        assert_eq!(format_time(951_782_400, 0), "2000-02-29 00:00", "leap day");
    }
}
