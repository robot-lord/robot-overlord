//! Timestamp display helpers shared by CLI and TUI (no chrono dependency).

use crate::types::now_ms;

/// `YYYY-MM-DD HH:MM:SSZ` from ms epoch (UTC).
pub fn fmt_ts(ms: i64) -> String {
    let secs = ms / 1000;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil date from days since epoch (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}Z")
}

/// Compact relative age: `12s`, `5m`, `3h`, `2d`, `6w`. `None` → `-`
/// (unknown is displayed as unknown, never as epoch).
pub fn rel_age(ms: Option<i64>) -> String {
    let Some(ms) = ms else {
        return "-".to_string();
    };
    let secs = ((now_ms() - ms) / 1000).max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else if secs < 7 * 86400 {
        format!("{}d", secs / 86400)
    } else {
        format!("{}w", secs / (7 * 86400))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date() {
        assert_eq!(fmt_ts(0), "1970-01-01 00:00:00Z");
        assert_eq!(fmt_ts(1_789_158_047_939), "2026-09-11 20:20:47Z");
    }

    #[test]
    fn ages() {
        assert_eq!(rel_age(None), "-");
        assert_eq!(rel_age(Some(now_ms() - 5_000)), "5s");
        assert_eq!(rel_age(Some(now_ms() - 3_600_000 * 5)), "5h");
    }
}
