//! Compact relative times for list rows ("5m ago").

use jiff::{Timestamp, tz::TimeZone};

/// `now - then` as a short phrase; dates older than 30 days are shown as
/// calendar dates in `tz` (`Mar 4` this year, `2023-03-04` otherwise).
pub fn relative(now: Timestamp, then: Timestamp, tz: &TimeZone) -> String {
    let secs = now.as_second() - then.as_second();
    match secs {
        i64::MIN..=59 => "just now".to_owned(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        86_400..=2_591_999 => format!("{}d ago", secs / 86_400),
        _ => {
            let then = then.to_zoned(tz.clone());
            if then.year() == now.to_zoned(tz.clone()).year() {
                then.strftime("%b %-d").to_string()
            } else {
                then.strftime("%Y-%m-%d").to_string()
            }
        }
    }
}

/// The time of day of `then` in `tz` (`14:05`), with the date (`Mar 4`
/// this year, `2023-03-04` otherwise) before it unless it is today.
pub fn clock(now: Timestamp, then: Timestamp, tz: &TimeZone) -> String {
    let (today, at) = (now.to_zoned(tz.clone()), then.to_zoned(tz.clone()));
    let time = at.strftime("%H:%M");
    if at.date() == today.date() {
        time.to_string()
    } else if at.year() == today.year() {
        format!("{} {time}", at.strftime("%b %-d"))
    } else {
        format!("{} {time}", at.strftime("%Y-%m-%d"))
    }
}

/// The heading of the day `then` falls on, in `tz`: "Today", "Yesterday",
/// a weekday and date within the last week (`Mon, Mar 4`), else the date
/// (`Mar 4` this year, `2023-03-04` otherwise).
pub fn day_label(now: Timestamp, then: Timestamp, tz: &TimeZone) -> String {
    let (today, day) = (now.to_zoned(tz.clone()), then.to_zoned(tz.clone()));
    let days = (today.date() - day.date()).get_days();
    match days {
        i32::MIN..=0 => "Today".to_owned(),
        1 => "Yesterday".to_owned(),
        2..=6 => day.strftime("%a, %b %-d").to_string(),
        _ if day.year() == today.year() => day.strftime("%b %-d").to_string(),
        _ => day.strftime("%Y-%m-%d").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_headings() {
        let now = ts("2024-06-15T12:00:00Z");
        let label = |then: &str| day_label(now, ts(then), &TimeZone::UTC);
        assert_eq!(label("2024-06-15T00:00:00Z"), "Today");
        assert_eq!(label("2024-06-16T09:00:00Z"), "Today");
        assert_eq!(label("2024-06-14T23:59:00Z"), "Yesterday");
        assert_eq!(label("2024-06-12T08:00:00Z"), "Wed, Jun 12");
        assert_eq!(label("2024-06-08T08:00:00Z"), "Jun 8");
        assert_eq!(label("2023-06-08T08:00:00Z"), "2023-06-08");
    }

    #[test]
    fn clock_times() {
        let now = ts("2024-06-15T12:00:00Z");
        let c = |then: &str| clock(now, ts(then), &TimeZone::UTC);
        assert_eq!(c("2024-06-15T09:05:00Z"), "09:05");
        assert_eq!(c("2024-06-14T23:59:00Z"), "Jun 14 23:59");
        assert_eq!(c("2023-06-14T08:00:00Z"), "2023-06-14 08:00");
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn rel(now: &str, then: &str) -> String {
        relative(ts(now), ts(then), &TimeZone::UTC)
    }

    #[test]
    fn buckets() {
        let now = "2024-06-15T12:00:00Z";
        assert_eq!(rel(now, "2024-06-15T12:00:00Z"), "just now");
        assert_eq!(rel(now, "2024-06-15T11:59:01Z"), "just now");
        assert_eq!(rel(now, "2024-06-15T11:59:00Z"), "1m ago");
        assert_eq!(rel(now, "2024-06-15T11:00:01Z"), "59m ago");
        assert_eq!(rel(now, "2024-06-15T11:00:00Z"), "1h ago");
        assert_eq!(rel(now, "2024-06-14T12:00:01Z"), "23h ago");
        assert_eq!(rel(now, "2024-06-14T12:00:00Z"), "1d ago");
        assert_eq!(rel(now, "2024-05-16T12:00:01Z"), "29d ago");
    }

    #[test]
    fn old_dates_are_calendar_dates() {
        let now = "2024-06-15T12:00:00Z";
        assert_eq!(rel(now, "2024-03-04T08:00:00Z"), "Mar 4");
        assert_eq!(rel(now, "2023-03-04T08:00:00Z"), "2023-03-04");
    }

    #[test]
    fn future_times_are_just_now() {
        assert_eq!(
            rel("2024-06-15T12:00:00Z", "2024-06-15T12:05:00Z"),
            "just now"
        );
    }
}
