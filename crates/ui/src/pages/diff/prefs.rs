//! The cycles of the diff preferences panel. Pure, so they can be tested
//! without a window.

use model::{DiffContext, Whitespace};

/// Context sizes the panel steps through, "whole file" last.
const CONTEXTS: [u32; 5] = [3, 10, 25, 50, 100];

/// The next larger context; after the last size comes the whole file, and
/// after that the smallest size again.
pub fn next_context(c: DiffContext) -> DiffContext {
    match c {
        DiffContext::All => DiffContext::Lines(CONTEXTS[0]),
        DiffContext::Lines(n) => CONTEXTS
            .iter()
            .find(|c| **c > n)
            .map_or(DiffContext::All, |c| DiffContext::Lines(*c)),
    }
}

pub fn next_whitespace(w: &Whitespace) -> Whitespace {
    match w {
        Whitespace::IgnoreNone => Whitespace::IgnoreTrailing,
        Whitespace::IgnoreTrailing => Whitespace::IgnoreLeadingAndTrailing,
        Whitespace::IgnoreLeadingAndTrailing => Whitespace::IgnoreAll,
        Whitespace::IgnoreAll | Whitespace::Unknown(_) => Whitespace::IgnoreNone,
    }
}

pub fn whitespace_label(w: &Whitespace) -> &'static str {
    match w {
        Whitespace::IgnoreNone => "show all",
        Whitespace::IgnoreTrailing => "ignore trailing",
        Whitespace::IgnoreLeadingAndTrailing => "ignore leading and trailing",
        Whitespace::IgnoreAll => "ignore all",
        Whitespace::Unknown(_) => "unknown",
    }
}

pub fn context_label(c: DiffContext) -> String {
    match c {
        DiffContext::Lines(n) => format!("{n} lines"),
        DiffContext::All => "whole file".to_owned(),
    }
}

/// The next tab width: 2, 4, 8, then 2 again.
pub fn next_tab_size(n: u32) -> u32 {
    match n {
        0..=1 => 2,
        2..=3 => 4,
        4..=7 => 8,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_steps_up_then_whole_file_then_wraps() {
        assert_eq!(next_context(DiffContext::Lines(3)), DiffContext::Lines(10));
        assert_eq!(next_context(DiffContext::Lines(10)), DiffContext::Lines(25));
        assert_eq!(next_context(DiffContext::Lines(100)), DiffContext::All);
        assert_eq!(next_context(DiffContext::All), DiffContext::Lines(3));
        // A value between the steps goes to the next step.
        assert_eq!(next_context(DiffContext::Lines(12)), DiffContext::Lines(25));
        assert_eq!(next_context(DiffContext::Lines(0)), DiffContext::Lines(3));
    }

    #[test]
    fn whitespace_cycles_through_all_modes() {
        let mut w = Whitespace::IgnoreNone;
        let mut seen = Vec::new();
        for _ in 0..4 {
            w = next_whitespace(&w);
            seen.push(w.clone());
        }
        assert_eq!(
            seen,
            [
                Whitespace::IgnoreTrailing,
                Whitespace::IgnoreLeadingAndTrailing,
                Whitespace::IgnoreAll,
                Whitespace::IgnoreNone
            ]
        );
    }

    #[test]
    fn tab_sizes() {
        assert_eq!(next_tab_size(2), 4);
        assert_eq!(next_tab_size(4), 8);
        assert_eq!(next_tab_size(8), 2);
        assert_eq!(next_tab_size(5), 8);
    }
}
