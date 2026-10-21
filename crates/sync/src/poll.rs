//! When to refresh a query. Pure functions of their inputs: no clock, no
//! timers, no runtime. The UI layer reads [`PollPolicy::delay`] and arms
//! its own timer.

use std::time::Duration;

use jiff::Timestamp;

/// Tunable intervals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollPolicy {
    /// Interval while the window is focused.
    pub focused: Duration,
    /// Interval while the window is not focused.
    pub unfocused: Duration,
    /// Upper bound of the interval after repeated failures.
    pub max_backoff: Duration,
}

impl Default for PollPolicy {
    fn default() -> Self {
        Self {
            focused: Duration::from_secs(60),
            unfocused: Duration::from_secs(5 * 60),
            max_backoff: Duration::from_secs(30 * 60),
        }
    }
}

/// What the policy needs to know about one query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollState {
    /// When the query was last refreshed or attempted (success or
    /// failure), if ever.
    pub last_attempt: Option<Timestamp>,
    pub window_focused: bool,
    /// Whether the query is on screen or otherwise active. Queries that
    /// are not are never polled.
    pub visible: bool,
    /// Failed attempts in a row; zero after a success.
    pub consecutive_failures: u32,
}

impl PollPolicy {
    /// The delay between attempts in this state: the base interval for the
    /// focus state, doubled per consecutive failure, capped at
    /// `max_backoff`.
    pub fn interval(&self, state: &PollState) -> Duration {
        let base = if state.window_focused {
            self.focused
        } else {
            self.unfocused
        };
        // 2^20 times any sane base is far past the cap; avoids overflow.
        let factor = 1u32 << state.consecutive_failures.min(20);
        base.saturating_mul(factor).min(self.max_backoff.max(base))
    }

    /// When the next refresh is due. `None` means "do not poll" (the query
    /// is not visible). A query never refreshed is due `now`. A time in
    /// the past means overdue.
    pub fn next_due(&self, now: Timestamp, state: &PollState) -> Option<Timestamp> {
        if !state.visible {
            return None;
        }
        let Some(last) = state.last_attempt else {
            return Some(now);
        };
        Some(last.checked_add(self.interval(state)).unwrap_or(last))
    }

    pub fn is_due(&self, now: Timestamp, state: &PollState) -> bool {
        self.next_due(now, state).is_some_and(|due| due <= now)
    }

    /// How long a timer should wait before refreshing; zero if overdue.
    /// `None` if the query should not be polled.
    pub fn delay(&self, now: Timestamp, state: &PollState) -> Option<Duration> {
        let due = self.next_due(now, state)?;
        Some(Duration::try_from(due.duration_since(now)).unwrap_or(Duration::ZERO))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn state(last: Option<&str>) -> PollState {
        PollState {
            last_attempt: last.map(ts),
            window_focused: true,
            visible: true,
            consecutive_failures: 0,
        }
    }

    const T0: &str = "2024-01-01T00:00:00Z";

    #[test]
    fn never_refreshed_is_due_now() {
        let p = PollPolicy::default();
        let now = ts(T0);
        assert_eq!(p.next_due(now, &state(None)), Some(now));
        assert!(p.is_due(now, &state(None)));
        assert_eq!(p.delay(now, &state(None)), Some(Duration::ZERO));
    }

    #[test]
    fn focused_interval() {
        let p = PollPolicy::default();
        let s = state(Some(T0));
        assert_eq!(p.next_due(ts(T0), &s), Some(ts("2024-01-01T00:01:00Z")));
        assert!(!p.is_due(ts("2024-01-01T00:00:59Z"), &s));
        assert!(p.is_due(ts("2024-01-01T00:01:00Z"), &s));
        assert_eq!(
            p.delay(ts("2024-01-01T00:00:15Z"), &s),
            Some(Duration::from_secs(45))
        );
    }

    #[test]
    fn unfocused_is_slower_and_refocus_shortens_it() {
        let p = PollPolicy::default();
        let mut s = state(Some(T0));
        s.window_focused = false;
        assert_eq!(p.next_due(ts(T0), &s), Some(ts("2024-01-01T00:05:00Z")));
        // Refocus after 2 minutes: the focused interval has already passed.
        s.window_focused = true;
        let now = ts("2024-01-01T00:02:00Z");
        assert!(p.is_due(now, &s));
        assert_eq!(p.delay(now, &s), Some(Duration::ZERO));
    }

    #[test]
    fn invisible_queries_are_not_polled() {
        let p = PollPolicy::default();
        let mut s = state(None);
        s.visible = false;
        assert_eq!(p.next_due(ts(T0), &s), None);
        assert!(!p.is_due(ts(T0), &s));
        assert_eq!(p.delay(ts(T0), &s), None);
    }

    #[test]
    fn failures_back_off_exponentially_with_a_cap() {
        let p = PollPolicy::default();
        let mut s = state(Some(T0));
        let secs = |n| Duration::from_secs(n);
        let expect = [60, 120, 240, 480, 960, 1800, 1800];
        for (failures, want) in expect.into_iter().enumerate() {
            s.consecutive_failures = failures as u32;
            assert_eq!(p.interval(&s), secs(want), "failures={failures}");
        }
        s.consecutive_failures = u32::MAX;
        assert_eq!(p.interval(&s), secs(1800));
        // A success resets it.
        s.consecutive_failures = 0;
        assert_eq!(p.interval(&s), secs(60));
    }

    #[test]
    fn backoff_never_drops_below_the_base_interval() {
        let p = PollPolicy {
            max_backoff: Duration::from_secs(10),
            ..PollPolicy::default()
        };
        let mut s = state(Some(T0));
        s.consecutive_failures = 3;
        assert_eq!(p.interval(&s), Duration::from_secs(60));
    }

    #[test]
    fn intervals_are_configurable() {
        let p = PollPolicy {
            focused: Duration::from_secs(10),
            unfocused: Duration::from_secs(20),
            max_backoff: Duration::from_secs(25),
        };
        let mut s = state(Some(T0));
        assert_eq!(p.interval(&s), Duration::from_secs(10));
        s.consecutive_failures = 2;
        assert_eq!(p.interval(&s), Duration::from_secs(25));
    }
}
