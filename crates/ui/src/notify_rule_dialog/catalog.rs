//! Which labels and vote values the rule editor offers: what the cache has
//! seen ([`store::Store::label_values`]), with fallbacks, and always what
//! the rule being edited already names.

use std::collections::{BTreeMap, BTreeSet};

/// The values a label is assumed to take when the cache has not seen it:
/// Gerrit's defaults.
pub(crate) fn fallback_range(label: &str) -> Vec<i32> {
    match label {
        "Code-Review" => vec![-2, -1, 0, 1, 2],
        _ => vec![-1, 0, 1],
    }
}

/// The labels to offer with the values each can take, sorted by name.
///
/// - Nothing seen yet (`seen` is `None` or empty): Code-Review and Verified
///   with their usual ranges.
/// - A label seen with no values (a list row names it) takes
///   [`fallback_range`].
/// - Every label in `keep` (those of the rule being edited, `"*"` aside) is
///   offered even if never seen.
pub(crate) fn offered_labels(
    seen: Option<&BTreeMap<String, BTreeSet<i32>>>,
    keep: &[String],
) -> Vec<(String, Vec<i32>)> {
    let mut labels: BTreeMap<String, BTreeSet<i32>> = seen.cloned().unwrap_or_default();
    if labels.is_empty() {
        for name in ["Code-Review", "Verified"] {
            labels.insert(name.to_owned(), BTreeSet::new());
        }
    }
    for name in keep.iter().filter(|l| l.as_str() != "*") {
        labels.entry(name.clone()).or_default();
    }
    labels
        .into_iter()
        .map(|(name, values)| {
            let values = if values.is_empty() {
                fallback_range(&name)
            } else {
                values.into_iter().collect()
            };
            (name, values)
        })
        .collect()
}

/// The value checkboxes: the union of the values of the labels that count
/// (all of them for "any label", else the `selected` ones), and always the
/// values already `chosen`, so that none can be hidden while it is set.
/// Ascending.
pub(crate) fn shown_values(
    offered: &[(String, Vec<i32>)],
    any_label: bool,
    selected: &BTreeSet<String>,
    chosen: &BTreeSet<i32>,
) -> Vec<i32> {
    let mut values: BTreeSet<i32> = chosen.clone();
    for (name, label_values) in offered {
        if any_label || selected.contains(name) {
            values.extend(label_values);
        }
    }
    values.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(list: &[(&str, &[i32])]) -> BTreeMap<String, BTreeSet<i32>> {
        list.iter()
            .map(|(l, v)| ((*l).to_owned(), v.iter().copied().collect()))
            .collect()
    }

    fn names(offered: &[(String, Vec<i32>)]) -> Vec<&str> {
        offered.iter().map(|(n, _)| n.as_str()).collect()
    }

    #[test]
    fn nothing_cached_offers_code_review_and_verified() {
        for s in [None, Some(&BTreeMap::new())] {
            let offered = offered_labels(s, &[]);
            assert_eq!(names(&offered), ["Code-Review", "Verified"]);
            assert_eq!(offered[0].1, [-2, -1, 0, 1, 2]);
            assert_eq!(offered[1].1, [-1, 0, 1]);
        }
    }

    #[test]
    fn cached_labels_are_offered_with_the_values_seen() {
        let s = seen(&[("Code-Review", &[-1, 1]), ("Custom", &[-3, 3])]);
        let offered = offered_labels(Some(&s), &[]);
        assert_eq!(
            offered,
            [
                ("Code-Review".to_owned(), vec![-1, 1]),
                ("Custom".to_owned(), vec![-3, 3])
            ]
        );
    }

    #[test]
    fn a_label_seen_without_values_gets_a_fallback_range() {
        let s = seen(&[("Code-Review", &[]), ("Presubmit", &[])]);
        let offered = offered_labels(Some(&s), &[]);
        assert_eq!(offered[0].1, [-2, -1, 0, 1, 2]);
        assert_eq!(offered[1], ("Presubmit".to_owned(), vec![-1, 0, 1]));
    }

    #[test]
    fn labels_of_the_rule_are_kept_even_when_unseen() {
        let s = seen(&[("Verified", &[-1, 1])]);
        let keep = vec!["Legacy".to_owned(), "*".to_owned()];
        let offered = offered_labels(Some(&s), &keep);
        assert_eq!(names(&offered), ["Legacy", "Verified"]);
        assert_eq!(offered[0].1, [-1, 0, 1]);
        // Nothing cached: the rule's label comes with the two defaults.
        let offered = offered_labels(None, &keep);
        assert_eq!(names(&offered), ["Code-Review", "Legacy", "Verified"]);
    }

    #[test]
    fn values_are_the_union_of_the_selected_labels_plus_the_chosen_ones() {
        let offered = offered_labels(None, &[]);
        let none = BTreeSet::new();
        let no_labels: BTreeSet<String> = BTreeSet::new();
        let cr: BTreeSet<String> = ["Code-Review".to_owned()].into();
        let both: BTreeSet<String> = ["Code-Review".to_owned(), "Verified".to_owned()].into();
        assert_eq!(shown_values(&offered, false, &cr, &none), [-2, -1, 0, 1, 2]);
        let verified: BTreeSet<String> = ["Verified".to_owned()].into();
        assert_eq!(shown_values(&offered, false, &verified, &none), [-1, 0, 1]);
        assert_eq!(
            shown_values(&offered, false, &both, &none),
            [-2, -1, 0, 1, 2]
        );
        // Any label: every label counts, whatever is selected.
        assert_eq!(
            shown_values(&offered, true, &no_labels, &none),
            [-2, -1, 0, 1, 2]
        );
        // No label selected: nothing is offered but what is chosen.
        assert!(shown_values(&offered, false, &no_labels, &none).is_empty());
        let chosen: BTreeSet<i32> = [-5, 1].into();
        assert_eq!(
            shown_values(&offered, false, &verified, &chosen),
            [-5, -1, 0, 1]
        );
    }
}
