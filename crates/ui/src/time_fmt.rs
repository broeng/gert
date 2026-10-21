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

#[cfg(test)]
mod tests {
    use super::*;

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
