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

/// `ms` in the user's time zone (None if the C library can't say).
fn local(ms: i64) -> Option<libc::tm> {
    let t: libc::time_t = (ms / 1000) as libc::time_t;
    // SAFETY: an all-zero `tm` is a valid value (plain integers and a
    // nullable pointer); `localtime_r` writes into the `tm` we pass and
    // reads only `t`, both live for the call.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        (!libc::localtime_r(&t, &mut tm).is_null()).then_some(tm)
    }
}

/// `3:00pm` in the user's time zone; with the weekday when that isn't
/// today (`Thu 9:00am`). Falls back to `fmt_ts`.
pub fn clock(ms: i64) -> String {
    match (local(ms), local(now_ms())) {
        (Some(t), Some(n)) => fmt_clock(
            t.tm_hour,
            t.tm_min,
            ((t.tm_yday, t.tm_year) != (n.tm_yday, n.tm_year)).then_some(t.tm_wday),
        ),
        _ => fmt_ts(ms),
    }
}

fn fmt_clock(hour: i32, min: i32, weekday: Option<i32>) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let (h, half) = match hour {
        0 => (12, "am"),
        1..=11 => (hour, "am"),
        12 => (12, "pm"),
        _ => (hour - 12, "pm"),
    };
    let day = weekday
        .and_then(|d| DAYS.get(d as usize))
        .map(|d| format!("{d} "))
        .unwrap_or_default();
    format!("{day}{h}:{min:02}{half}")
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

    #[test]
    fn clock_times() {
        assert_eq!(fmt_clock(15, 0, None), "3:00pm");
        assert_eq!(fmt_clock(0, 5, None), "12:05am");
        assert_eq!(fmt_clock(12, 30, None), "12:30pm");
        assert_eq!(fmt_clock(9, 0, Some(4)), "Thu 9:00am");
        assert!(!clock(now_ms()).is_empty());
    }
}
