//! Smart date entry: `today`, `yesterday`, `-3`, `15`, `3/15`, `2026-03-15`, `20260315`.

use chrono::{Datelike, Days, NaiveDate};

/// Parse `input` relative to `today`; partial dates are completed from `default`.
pub fn parse_smart_date(input: &str, today: NaiveDate, default: NaiveDate) -> Option<NaiveDate> {
    let s = input.trim().to_ascii_lowercase();
    match s.as_str() {
        "" => return Some(default),
        "today" | "t" | "." => return Some(today),
        "yesterday" | "y" => return today.checked_sub_days(Days::new(1)),
        "tomorrow" => return today.checked_add_days(Days::new(1)),
        _ => {}
    }
    if let Some(n) = s.strip_prefix('-').and_then(|n| n.parse::<u64>().ok()) {
        return today.checked_sub_days(Days::new(n));
    }
    if let Some(n) = s.strip_prefix('+').and_then(|n| n.parse::<u64>().ok()) {
        return today.checked_add_days(Days::new(n));
    }
    if s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit()) {
        let (y, m, d) = (
            s[..4].parse().ok()?,
            s[4..6].parse().ok()?,
            s[6..].parse().ok()?,
        );
        return NaiveDate::from_ymd_opt(y, m, d);
    }
    let parts: Vec<&str> = s.split(['-', '/', '.']).collect();
    let nums: Option<Vec<u32>> = parts.iter().map(|p| p.parse().ok()).collect();
    match nums?.as_slice() {
        [d] => NaiveDate::from_ymd_opt(default.year(), default.month(), *d),
        [m, d] => NaiveDate::from_ymd_opt(default.year(), *m, *d),
        [y, m, d] if parts[0].len() == 4 => NaiveDate::from_ymd_opt(*y as i32, *m, *d),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_dates() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let def = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
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
    }
}
