//! Due-date token parsing for inline `@<token>` syntax on checkbox lines.
//!
//! Tokens accepted (case-insensitive for words, strict for ISO):
//!   - `YYYY-MM-DD`             absolute date (local midnight)
//!   - `YYYY-MM-DD HH:MM`       absolute date + 24-hour local time
//!   - `today`, `tomorrow`      relative; resolved against `today`
//!   - `monday`..`sunday`       relative; today's name → today, else next
//!                              occurrence within 7 days
//!
//! Storage is always Unix-ms in UTC. Display labels live in the frontend.

use chrono::{Datelike, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Weekday};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DueDate {
    pub timestamp_ms: i64,
    pub has_time: bool,
}

/// Parse a `@<token>` payload (the part after the `@`) against a reference
/// `today`. Returns `None` if the token doesn't match any accepted form.
pub fn parse_due_token(token: &str, today: NaiveDate) -> Option<DueDate> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }

    if let Some(due) = try_parse_absolute(token) {
        return Some(due);
    }

    let lower = token.to_ascii_lowercase();
    let target_date = match lower.as_str() {
        "today" => today,
        "tomorrow" => today.succ_opt()?,
        other => weekday_from_str(other).map(|w| next_weekday(today, w))?,
    };
    Some(local_midnight_to_due(target_date, false))
}

/// Parse only the absolute ISO forms (`YYYY-MM-DD` or `YYYY-MM-DD HH:MM`).
/// Resolves absolute tokens without a `today` reference.
pub fn try_parse_absolute(token: &str) -> Option<DueDate> {
    parse_iso(token.trim())
}

/// Returns true for `today`, `tomorrow`, `<weekday>`. False for ISO forms or
/// garbage. Used by the rewrite-on-save path to decide which tokens get
/// substituted into the file.
pub fn is_relative(token: &str) -> bool {
    let lower = token.trim().to_ascii_lowercase();
    matches!(lower.as_str(), "today" | "tomorrow")
        || weekday_from_str(&lower).is_some()
}

/// Render an absolute `DueDate` back to its canonical token form.
pub fn render_absolute(due: &DueDate) -> String {
    // Build the local datetime back out of the UTC ms so the formatted
    // output matches what the user typed (or a relative resolution).
    let dt = Local.timestamp_millis_opt(due.timestamp_ms).single()
        .map(|d| d.naive_local())
        .unwrap_or_else(|| NaiveDateTime::default());
    if due.has_time {
        dt.format("%Y-%m-%d %H:%M").to_string()
    } else {
        dt.date().format("%Y-%m-%d").to_string()
    }
}

// ---- internals ----------------------------------------------------------

fn parse_iso(token: &str) -> Option<DueDate> {
    let (date_part, time_part) = match token.split_once(' ') {
        Some((d, t)) => (d, Some(t)),
        None => (token, None),
    };
    let date = NaiveDate::parse_from_str(date_part, "%Y-%m-%d").ok()?;
    match time_part {
        Some(t) => {
            let time = NaiveTime::parse_from_str(t, "%H:%M").ok()?;
            Some(local_datetime_to_due(NaiveDateTime::new(date, time), true))
        }
        None => Some(local_midnight_to_due(date, false)),
    }
}

fn weekday_from_str(s: &str) -> Option<Weekday> {
    match s {
        "monday" | "mon" => Some(Weekday::Mon),
        "tuesday" | "tue" => Some(Weekday::Tue),
        "wednesday" | "wed" => Some(Weekday::Wed),
        "thursday" | "thu" => Some(Weekday::Thu),
        "friday" | "fri" => Some(Weekday::Fri),
        "saturday" | "sat" => Some(Weekday::Sat),
        "sunday" | "sun" => Some(Weekday::Sun),
        _ => None,
    }
}

/// Today's name → today; otherwise the next calendar day matching `target`
/// within the next 7 days. Never returns yesterday.
fn next_weekday(today: NaiveDate, target: Weekday) -> NaiveDate {
    let today_dow = today.weekday().num_days_from_monday() as i64;
    let target_dow = target.num_days_from_monday() as i64;
    let mut diff = target_dow - today_dow;
    if diff < 0 {
        diff += 7;
    }
    today + chrono::Duration::days(diff)
}

fn local_midnight_to_due(date: NaiveDate, has_time: bool) -> DueDate {
    let dt = NaiveDateTime::new(date, NaiveTime::from_hms_opt(0, 0, 0).unwrap());
    local_datetime_to_due(dt, has_time)
}

fn local_datetime_to_due(naive: NaiveDateTime, has_time: bool) -> DueDate {
    // `single()` returns None for ambiguous (DST fall-back) times. Pick the
    // earliest interpretation in that case so the result is deterministic.
    let utc_ms = match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => dt.timestamp_millis(),
        chrono::LocalResult::Ambiguous(earliest, _) => earliest.timestamp_millis(),
        chrono::LocalResult::None => {
            // Spring-forward: shift by an hour to land on a valid local time.
            let bumped = naive + chrono::Duration::hours(1);
            Local
                .from_local_datetime(&bumped)
                .single()
                .map(|d| d.timestamp_millis())
                .unwrap_or(0)
        }
    };
    DueDate {
        timestamp_ms: utc_ms,
        has_time,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn iso_date_parses_to_local_midnight() {
        let today = d(2026, 5, 7);
        let due = parse_due_token("2026-05-15", today).unwrap();
        assert!(!due.has_time);
        // Round-trip
        assert_eq!(render_absolute(&due), "2026-05-15");
    }

    #[test]
    fn iso_date_with_time_parses_and_round_trips() {
        let today = d(2026, 5, 7);
        let due = parse_due_token("2026-05-15 09:30", today).unwrap();
        assert!(due.has_time);
        assert_eq!(render_absolute(&due), "2026-05-15 09:30");
    }

    #[test]
    fn today_resolves_to_today() {
        let today = d(2026, 5, 7);
        let due = parse_due_token("today", today).unwrap();
        assert_eq!(render_absolute(&due), "2026-05-07");
    }

    #[test]
    fn tomorrow_resolves_to_succ() {
        let today = d(2026, 5, 7);
        let due = parse_due_token("tomorrow", today).unwrap();
        assert_eq!(render_absolute(&due), "2026-05-08");
    }

    #[test]
    fn weekday_today_name_returns_today() {
        // 2026-05-07 is a Thursday.
        let today = d(2026, 5, 7);
        let due = parse_due_token("thursday", today).unwrap();
        assert_eq!(render_absolute(&due), "2026-05-07");
    }

    #[test]
    fn weekday_returns_next_within_a_week() {
        // 2026-05-07 is a Thursday → friday is +1 day.
        let today = d(2026, 5, 7);
        let due = parse_due_token("friday", today).unwrap();
        assert_eq!(render_absolute(&due), "2026-05-08");
    }

    #[test]
    fn weekday_wraps_to_next_week() {
        // 2026-05-07 is a Thursday → wednesday is +6 days.
        let today = d(2026, 5, 7);
        let due = parse_due_token("wednesday", today).unwrap();
        assert_eq!(render_absolute(&due), "2026-05-13");
    }

    #[test]
    fn weekday_short_form_accepted() {
        let today = d(2026, 5, 7);
        let due = parse_due_token("fri", today).unwrap();
        assert_eq!(render_absolute(&due), "2026-05-08");
    }

    #[test]
    fn case_insensitive_words() {
        let today = d(2026, 5, 7);
        assert!(parse_due_token("Today", today).is_some());
        assert!(parse_due_token("TOMORROW", today).is_some());
        assert!(parse_due_token("Friday", today).is_some());
    }

    #[test]
    fn garbage_returns_none() {
        let today = d(2026, 5, 7);
        assert!(parse_due_token("notadate", today).is_none());
        assert!(parse_due_token("", today).is_none());
        assert!(parse_due_token("2026-13-40", today).is_none());
        assert!(parse_due_token("2026-05-15 25:00", today).is_none());
        assert!(parse_due_token("2026/05/15", today).is_none());
    }

    #[test]
    fn is_relative_recognizes_relative_only() {
        assert!(is_relative("today"));
        assert!(is_relative("tomorrow"));
        assert!(is_relative("Friday"));
        assert!(is_relative("mon"));
        assert!(!is_relative("2026-05-15"));
        assert!(!is_relative("2026-05-15 09:00"));
        assert!(!is_relative("garbage"));
    }
}
