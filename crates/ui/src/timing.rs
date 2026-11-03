//! Where a slow load spends its time: debug lines under the [`TARGET`]
//! log target (`RUST_LOG=ui::timing=debug`), one per load, each naming
//! its steps and how long they took.

use std::time::{Duration, Instant};

/// The log target of every timing line.
pub const TARGET: &str = "ui::timing";

/// The named steps of one load, each timed from the end of the one before.
/// Plain data, so a background job can take it and hand it back.
pub struct Steps {
    start: Instant,
    last: Instant,
    parts: Vec<(&'static str, Duration)>,
}

impl Steps {
    pub fn start() -> Self {
        let now = Instant::now();
        Self {
            start: now,
            last: now,
            parts: Vec::new(),
        }
    }

    /// Ends the step `name` now.
    pub fn lap(&mut self, name: &'static str) {
        let now = Instant::now();
        self.parts.push((name, now - self.last));
        self.last = now;
    }

    pub fn total(&self) -> Duration {
        self.last - self.start
    }
}

impl std::fmt::Display for Steps {
    /// `12.3ms (store 0.1ms, git 12.2ms)`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.1?} (", self.total())?;
        for (i, (name, took)) in self.parts.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{name} {took:.1?}")?;
        }
        f.write_str(")")
    }
}
