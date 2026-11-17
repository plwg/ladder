//! Smart date entry: `today`, `yesterday`, `-3`, `15`, `3/15`, `2026-03-15`, `20260315`.

use jiff::Span;
use jiff::civil::Date;

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
    let year = default.year().into();
    match nums?.as_slice() {
        [d] => ymd(year, default.month().into(), *d),
        [m, d] => ymd(year, *m, *d),
        [y, m, d] if parts[0].len() == 4 => ymd(*y, *m, *d),
        _ => None,
    }
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
}
