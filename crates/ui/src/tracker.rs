//! Per-query bookkeeping for [`sync::PollPolicy`]: when the last attempt
//! was and how many failed in a row.

use jiff::Timestamp;
use sync::PollState;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tracker {
    last_attempt: Option<Timestamp>,
    failures: u32,
}

impl Tracker {
    pub fn success(&mut self, now: Timestamp) {
        self.last_attempt = Some(now);
        self.failures = 0;
    }

    pub fn failure(&mut self, now: Timestamp) {
        self.last_attempt = Some(now);
        self.failures = self.failures.saturating_add(1);
    }

    /// Makes the next policy check report "due", e.g. for a manual refresh.
    pub fn invalidate(&mut self) {
        self.last_attempt = None;
    }

    #[cfg(test)]
    pub fn failures(&self) -> u32 {
        self.failures
    }

    pub fn state(&self, window_focused: bool, visible: bool) -> PollState {
        PollState {
            last_attempt: self.last_attempt,
            window_focused,
            visible,
            consecutive_failures: self.failures,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sync::PollPolicy;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn fresh_tracker_is_due_immediately() {
        let t = Tracker::default();
        let now = ts("2024-01-01T00:00:00Z");
        assert!(PollPolicy::default().is_due(now, &t.state(true, true)));
    }

    #[test]
    fn success_waits_one_interval_failure_backs_off() {
        let p = PollPolicy::default();
        let t0 = ts("2024-01-01T00:00:00Z");
        let mut t = Tracker::default();
        t.success(t0);
        assert!(!p.is_due(ts("2024-01-01T00:00:59Z"), &t.state(true, true)));
        assert!(p.is_due(ts("2024-01-01T00:01:00Z"), &t.state(true, true)));
        t.failure(t0);
        t.failure(t0);
        assert_eq!(t.failures(), 2);
        assert!(!p.is_due(ts("2024-01-01T00:03:59Z"), &t.state(true, true)));
        assert!(p.is_due(ts("2024-01-01T00:04:00Z"), &t.state(true, true)));
        t.success(t0);
        assert_eq!(t.failures(), 0);
    }

    #[test]
    fn unfocused_windows_wait_longer() {
        let p = PollPolicy::default();
        let mut t = Tracker::default();
        t.success(ts("2024-01-01T00:00:00Z"));
        let now = ts("2024-01-01T00:02:00Z");
        assert!(p.is_due(now, &t.state(true, true)));
        assert!(!p.is_due(now, &t.state(false, true)));
    }

    #[test]
    fn invalidate_forces_a_refresh() {
        let mut t = Tracker::default();
        let now = ts("2024-01-01T00:00:00Z");
        t.success(now);
        t.invalidate();
        assert!(PollPolicy::default().is_due(now, &t.state(true, true)));
    }
}
