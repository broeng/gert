//! Label columns and vote markers in change lists (Gerrit semantics).

use std::collections::BTreeSet;

use model::{ChangeSummary, LabelSummary};

/// What a label cell shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mark {
    /// Max vote: green check.
    Approved,
    /// Min vote (blocking): red cross.
    Rejected,
    /// Positive vote that is not the max: green `+1`.
    Recommended(i32),
    /// Negative vote that is not the min: red `-1`.
    Disliked(i32),
    None,
}

/// The marker for a label. A blocking vote wins over an approval, as in
/// the Gerrit web UI.
pub fn mark(l: &LabelSummary) -> Mark {
    let value = |default: i32, positive: bool| match l.value {
        Some(v) if v != 0 && (v > 0) == positive => v,
        _ => default,
    };
    if l.rejected {
        Mark::Rejected
    } else if l.approved {
        Mark::Approved
    } else if l.disliked {
        Mark::Disliked(value(-1, false))
    } else if l.recommended {
        Mark::Recommended(value(1, true))
    } else {
        Mark::None
    }
}

impl Mark {
    pub fn text(&self) -> String {
        match self {
            Mark::Approved => "✓".into(),
            Mark::Rejected => "✗".into(),
            Mark::Recommended(v) => format!("+{v}"),
            Mark::Disliked(v) => format!("{v}"),
            Mark::None => String::new(),
        }
    }
}

/// The label columns to show for a set of rows: the labels present in any
/// row, Code-Review and Verified first, the rest alphabetical.
pub fn label_columns<'a>(rows: impl IntoIterator<Item = &'a ChangeSummary>) -> Vec<String> {
    let names: BTreeSet<&str> = rows
        .into_iter()
        .flat_map(|c| c.labels.keys().map(String::as_str))
        .collect();
    let mut cols: Vec<String> = Vec::new();
    for first in ["Code-Review", "Verified"] {
        if names.contains(first) {
            cols.push(first.to_owned());
        }
    }
    cols.extend(
        names
            .into_iter()
            .filter(|n| !matches!(*n, "Code-Review" | "Verified"))
            .map(str::to_owned),
    );
    cols
}

/// Column header for a label: `CR`, `V`, otherwise the initials of its
/// dash-separated words.
pub fn short_name(label: &str) -> String {
    match label {
        "Code-Review" => "CR".into(),
        "Verified" => "V".into(),
        other => {
            let initials: String = other
                .split(['-', ' ', '_'])
                .filter_map(|w| w.chars().next())
                .flat_map(char::to_uppercase)
                .collect();
            if initials.is_empty() {
                other.chars().take(2).collect()
            } else {
                initials
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::summary;

    fn l(f: impl FnOnce(&mut LabelSummary)) -> LabelSummary {
        let mut s = LabelSummary::default();
        f(&mut s);
        s
    }

    #[test]
    fn marks() {
        assert_eq!(mark(&l(|s| s.approved = true)), Mark::Approved);
        assert_eq!(mark(&l(|s| s.rejected = true)), Mark::Rejected);
        assert_eq!(mark(&l(|s| s.recommended = true)), Mark::Recommended(1));
        assert_eq!(mark(&l(|s| s.disliked = true)), Mark::Disliked(-1));
        assert_eq!(mark(&LabelSummary::default()), Mark::None);
    }

    #[test]
    fn blocking_vote_beats_approval() {
        assert_eq!(
            mark(&l(|s| {
                s.approved = true;
                s.rejected = true;
            })),
            Mark::Rejected
        );
    }

    #[test]
    fn value_is_used_when_it_agrees_with_the_slot() {
        assert_eq!(
            mark(&l(|s| {
                s.recommended = true;
                s.value = Some(2)
            })),
            Mark::Recommended(2)
        );
        // A stale or contradictory value falls back to +-1.
        assert_eq!(
            mark(&l(|s| {
                s.disliked = true;
                s.value = Some(1)
            })),
            Mark::Disliked(-1)
        );
    }

    #[test]
    fn mark_text() {
        assert_eq!(Mark::Approved.text(), "✓");
        assert_eq!(Mark::Rejected.text(), "✗");
        assert_eq!(Mark::Recommended(1).text(), "+1");
        assert_eq!(Mark::Disliked(-1).text(), "-1");
        assert_eq!(Mark::None.text(), "");
    }

    #[test]
    fn columns_are_ordered_and_deduplicated() {
        let mut a = summary(1);
        a.labels.insert("Zeta".into(), LabelSummary::default());
        a.labels.insert("Verified".into(), LabelSummary::default());
        let mut b = summary(2);
        b.labels
            .insert("Code-Review".into(), LabelSummary::default());
        b.labels.insert("Alpha".into(), LabelSummary::default());
        b.labels.insert("Zeta".into(), LabelSummary::default());
        assert_eq!(
            label_columns([&a, &b]),
            ["Code-Review", "Verified", "Alpha", "Zeta"]
        );
        assert!(label_columns([&summary(3)]).is_empty());
    }

    #[test]
    fn short_names() {
        assert_eq!(short_name("Code-Review"), "CR");
        assert_eq!(short_name("Verified"), "V");
        assert_eq!(short_name("Presubmit-Ready"), "PR");
        assert_eq!(short_name("Commit-Queue"), "CQ");
    }
}
