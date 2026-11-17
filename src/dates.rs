//! Smart date entry: `today`, `yesterday`, `-3`, `15`, `3/15`, `2026-03-15`, `20260315`,
//! `fri`, `last fri`, `next fri`, `som`, `eom`, `2w ago`, `-1mo`, `1 month 3 days`.

use jiff::Span;
use jiff::civil::{Date, Weekday};

/// Parse `input` relative to `today`; partial dates are completed from `default`.
pub fn parse_smart_date(input: &str, today: Date, default: Date) -> Option<Date> {
    let s = input.trim().to_ascii_lowercase();
    match s.as_str() {
        "" => return Some(default),
        "today" | "t" | "." => return Some(today),
        "yesterday" | "y" => return offset(today, -1),
        "tomorrow" => return offset(today, 1),
        _ => {}
    }
    if let Some(n) = s.strip_prefix('-').and_then(|n| n.parse::<u32>().ok()) {
        return offset(today, -i64::from(n));
    }
    if let Some(n) = s.strip_prefix('+').and_then(|n| n.parse::<u32>().ok()) {
        return offset(today, i64::from(n));
    }
    if s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit()) {
        let (y, m, d) = (
            s[..4].parse().ok()?,
            s[4..6].parse().ok()?,
            s[6..].parse().ok()?,
        );
        return ymd(y, m, d);
    }
    let parts: Vec<&str> = s.split(['-', '/', '.']).collect();
    let nums: Option<Vec<i32>> = parts.iter().map(|p| p.parse().ok()).collect();
    if let Some(nums) = nums {
        let year = default.year().into();
        return match nums.as_slice() {
            [d] => ymd(year, default.month().into(), *d),
            [m, d] => ymd(year, *m, *d),
            [y, m, d] if parts[0].len() == 4 => ymd(*y, *m, *d),
            _ => None,
        };
    }
    match s.as_str() {
        "som" => return Some(today.first_of_month()),
        "eom" => return Some(today.last_of_month()),
        _ => {}
    }
    if let Some(d) = weekday_date(&s, today) {
        return Some(d);
    }
    relative_span(&s, today)
}

/// `fri` (the latest Friday, today included), `last fri` (before today) or
/// `next fri` (after today).
fn weekday_date(s: &str, today: Date) -> Option<Date> {
    let (nth, name) = match s.split_once(' ') {
        Some(("last", name)) => (-1, name),
        Some(("next", name)) => (1, name),
        Some(_) => return None,
        None if weekday(s)? == today.weekday() => return Some(today),
        None => (-1, s),
    };
    today.nth_weekday(nth, weekday(name.trim())?).ok()
}

/// A weekday from its name or an abbreviation of at least three letters.
fn weekday(name: &str) -> Option<Weekday> {
    const DAYS: [(&str, Weekday); 7] = [
        ("monday", Weekday::Monday),
        ("tuesday", Weekday::Tuesday),
        ("wednesday", Weekday::Wednesday),
        ("thursday", Weekday::Thursday),
        ("friday", Weekday::Friday),
        ("saturday", Weekday::Saturday),
        ("sunday", Weekday::Sunday),
    ];
    if name.len() < 3 {
        return None;
    }
    DAYS.iter()
        .find(|(full, _)| full.starts_with(name))
        .map(|&(_, w)| w)
}

/// A calendar span from `today`: `3d`, `2w ago`, `-1mo`, `1 month 3 days`, `1y`.
/// Months and years that land past a month's end clamp to its last day.
fn relative_span(s: &str, today: Date) -> Option<Date> {
    let span: Span = s.parse().ok()?;
    let has_time = span.get_hours() != 0
        || span.get_minutes() != 0
        || span.get_seconds() != 0
        || span.get_milliseconds() != 0
        || span.get_microseconds() != 0
        || span.get_nanoseconds() != 0;
    if has_time {
        return None;
    }
    today.checked_add(span).ok()
}

/// `today` shifted by `days`, or `None` outside jiff's supported range.
fn offset(today: Date, days: i64) -> Option<Date> {
    today.checked_add(Span::new().try_days(days).ok()?).ok()
}

fn ymd(y: i32, m: i32, d: i32) -> Option<Date> {
    Date::new(y.try_into().ok()?, m.try_into().ok()?, d.try_into().ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    #[test]
    fn smart_dates() {
        let today = date(2026, 10, 19);
        let def = date(2026, 9, 1);
        let d = |s| parse_smart_date(s, today, def).map(|d| d.to_string());
        assert_eq!(d("").as_deref(), Some("2026-09-01"));
        assert_eq!(d("y").as_deref(), Some("2026-10-18"));
        assert_eq!(d("-7").as_deref(), Some("2026-10-12"));
        assert_eq!(d("15").as_deref(), Some("2026-09-15"));
        assert_eq!(d("3/5").as_deref(), Some("2026-03-05"));
        assert_eq!(d("2025-12-31").as_deref(), Some("2025-12-31"));
        assert_eq!(d("2025/1/2").as_deref(), Some("2025-01-02"));
        assert_eq!(d("20240229").as_deref(), Some("2024-02-29"));
        assert_eq!(d("2/30"), None);
        assert_eq!(d("bogus"), None);
        assert_eq!(d("-99999999"), None);
        assert_eq!(d("+99999999"), None);
    }

    #[test]
    fn weekdays_and_month_ends() {
        // 2026-10-19 is a Monday.
        let today = date(2026, 10, 19);
        let d = |s| parse_smart_date(s, today, today).map(|d| d.to_string());
        assert_eq!(d("mon").as_deref(), Some("2026-10-19"));
        assert_eq!(d("Monday").as_deref(), Some("2026-10-19"));
        assert_eq!(d("last mon").as_deref(), Some("2026-10-12"));
        assert_eq!(d("next mon").as_deref(), Some("2026-10-26"));
        assert_eq!(d("fri").as_deref(), Some("2026-10-16"));
        assert_eq!(d("thurs").as_deref(), Some("2026-10-15"));
        assert_eq!(d("last fri").as_deref(), Some("2026-10-16"));
        assert_eq!(d("next fri").as_deref(), Some("2026-10-23"));
        assert_eq!(d("som").as_deref(), Some("2026-10-01"));
        assert_eq!(d("eom").as_deref(), Some("2026-10-31"));
        assert_eq!(d("fr"), None);
        assert_eq!(d("fridays"), None);
        assert_eq!(d("this fri"), None);
    }

    #[test]
    fn relative_spans() {
        let today = date(2026, 1, 31);
        let d = |s| parse_smart_date(s, today, today).map(|d| d.to_string());
        assert_eq!(d("3d").as_deref(), Some("2026-02-03"));
        assert_eq!(d("2w ago").as_deref(), Some("2026-01-17"));
        assert_eq!(d("2 weeks ago").as_deref(), Some("2026-01-17"));
        assert_eq!(d("1mo").as_deref(), Some("2026-02-28"));
        assert_eq!(d("-1 month").as_deref(), Some("2025-12-31"));
        assert_eq!(d("1 month 3 days").as_deref(), Some("2026-03-03"));
        assert_eq!(d("1y ago").as_deref(), Some("2025-01-31"));
        assert_eq!(d("1h"), None);
        assert_eq!(d("1d 2h"), None);
    }
}
