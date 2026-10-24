//! The pure part of the notification rule editor: what the form's controls
//! hold, the rule they make, and what is wrong with it.

use std::collections::BTreeSet;

use sync::notify::{Rule, RuleEvent, Scope};

/// Every event, in the order the editor offers them.
pub(crate) const EVENTS: [RuleEvent; 10] = [
    RuleEvent::Vote,
    RuleEvent::Comment,
    RuleEvent::AttentionAdded,
    RuleEvent::ReviewerAdded,
    RuleEvent::PatchSet,
    RuleEvent::Merged,
    RuleEvent::Abandoned,
    RuleEvent::ReadyToSubmit,
    RuleEvent::MergeConflict,
    RuleEvent::Mention,
];

pub(crate) fn event_label(event: RuleEvent) -> &'static str {
    match event {
        RuleEvent::Vote => "Vote",
        RuleEvent::Comment => "Comment",
        RuleEvent::AttentionAdded => "Attention set",
        RuleEvent::ReviewerAdded => "Added as reviewer",
        RuleEvent::PatchSet => "New patch set",
        RuleEvent::Merged => "Merged",
        RuleEvent::Abandoned => "Abandoned",
        RuleEvent::ReadyToSubmit => "Ready to submit",
        RuleEvent::MergeConflict => "Merge conflict",
        RuleEvent::Mention => "Mention",
    }
}

pub(crate) fn scope_label(scope: &Scope) -> String {
    match scope {
        Scope::Owner => "My changes".to_owned(),
        Scope::Reviewer => "I review".to_owned(),
        Scope::Cc => "I am CC'd".to_owned(),
        Scope::Attention => "My attention set".to_owned(),
        Scope::Starred => "Starred".to_owned(),
        Scope::Any => "Any change".to_owned(),
        Scope::Query(name) => format!("Query: {name}"),
    }
}

/// The scopes to choose from: the simple ones, then each saved query by
/// name, then the rule's own query if no configured one has that name (so
/// that it can be seen and left).
pub(crate) fn scope_choices(queries: &[String], current: &Scope) -> Vec<Scope> {
    let mut scopes = vec![
        Scope::Owner,
        Scope::Reviewer,
        Scope::Cc,
        Scope::Attention,
        Scope::Starred,
        Scope::Any,
    ];
    scopes.extend(queries.iter().map(|q| Scope::Query(q.clone())));
    if matches!(current, Scope::Query(_)) && !scopes.contains(current) {
        scopes.push(current.clone());
    }
    scopes
}

/// What the form holds besides the two text fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Form {
    pub event: RuleEvent,
    pub scope: Scope,
    /// Slugs; empty: all servers.
    pub servers: Vec<String>,
    pub any_label: bool,
    pub labels: BTreeSet<String>,
    pub any_value: bool,
    pub values: BTreeSet<i32>,
    pub exclude_bots: bool,
    pub enabled: bool,
}

/// A new rule starts as "comments on my changes".
pub(crate) fn new_rule() -> Rule {
    Rule::new(RuleEvent::Comment, Scope::Owner)
}

impl Form {
    pub(crate) fn from_rule(rule: &Rule) -> Self {
        Self {
            event: rule.event,
            scope: rule.scope.clone(),
            servers: rule.servers.clone(),
            any_label: rule.labels.is_empty() || rule.labels.iter().any(|l| l == "*"),
            labels: rule
                .labels
                .iter()
                .filter(|l| l.as_str() != "*")
                .cloned()
                .collect(),
            any_value: rule.values.is_empty(),
            values: rule.values.iter().copied().collect(),
            exclude_bots: rule.exclude_bots,
            enabled: rule.enabled,
        }
    }

    pub(crate) fn is_vote(&self) -> bool {
        self.event == RuleEvent::Vote
    }

    /// The rule: `base` (which carries what the editor does not offer, like
    /// the comment flags), the form and the two texts. Labels and values
    /// are for vote rules only; "any" is the empty list.
    pub(crate) fn build(&self, base: &Rule, name: &str, from: &str) -> Rule {
        let vote = self.is_vote();
        Rule {
            name: Some(name.trim().to_owned()).filter(|n| !n.is_empty()),
            event: self.event,
            scope: self.scope.clone(),
            servers: self.servers.clone(),
            labels: if vote && !self.any_label {
                self.labels.iter().cloned().collect()
            } else {
                Vec::new()
            },
            values: if vote && !self.any_value {
                self.values.iter().copied().collect()
            } else {
                Vec::new()
            },
            from: from
                .split(',')
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .map(str::to_owned)
                .collect(),
            exclude_bots: self.exclude_bots,
            enabled: self.enabled,
            ..base.clone()
        }
    }

    /// What keeps the form from being saved, if anything.
    pub(crate) fn problem(&self) -> Option<&'static str> {
        if !self.is_vote() {
            return None;
        }
        if !self.any_label && self.labels.is_empty() {
            return Some("Tick at least one label, or Any label.");
        }
        if !self.any_value && self.values.is_empty() {
            return Some("Tick at least one value, or Any value.");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vote_rule(labels: &[&str], values: &[i32]) -> Rule {
        let mut r = Rule::new(RuleEvent::Vote, Scope::Owner);
        r.labels = labels.iter().map(|l| (*l).to_owned()).collect();
        r.values = values.to_vec();
        r
    }

    #[test]
    fn a_rule_round_trips_through_the_form() {
        let mut rule = vote_rule(&["Code-Review"], &[-2, -1]);
        rule.name = Some("Negative".into());
        rule.servers = vec!["a".into()];
        rule.from = vec!["bob".into(), "carol".into()];
        rule.exclude_bots = false;
        rule.enabled = false;
        rule.only_unresolved = true;
        let form = Form::from_rule(&rule);
        assert!(!form.any_label && !form.any_value);
        assert_eq!(form.build(&rule, "Negative", "bob, carol"), rule);
    }

    #[test]
    fn empty_and_star_labels_and_empty_values_mean_any() {
        let form = Form::from_rule(&vote_rule(&[], &[]));
        assert!(form.any_label && form.any_value);
        let form = Form::from_rule(&vote_rule(&["*"], &[1]));
        assert!(form.any_label && !form.any_value);
        assert!(form.labels.is_empty());
        let rule = form.build(&vote_rule(&["*"], &[1]), "", "");
        assert!(rule.labels.is_empty());
        assert_eq!(rule.values, [1]);
    }

    #[test]
    fn only_vote_rules_keep_labels_and_values() {
        let base = vote_rule(&["Code-Review"], &[-1]);
        let mut form = Form::from_rule(&base);
        form.event = RuleEvent::Comment;
        let rule = form.build(&base, "", "");
        assert!(rule.labels.is_empty() && rule.values.is_empty());
        // The hidden choice is still in the form, so switching back finds it.
        form.event = RuleEvent::Vote;
        assert_eq!(form.build(&base, "", "").labels, ["Code-Review"]);
    }

    #[test]
    fn a_blank_name_is_none_and_the_actors_are_trimmed() {
        let base = new_rule();
        let rule = Form::from_rule(&base).build(&base, "   ", " a ,, b@x.org , ");
        assert_eq!(rule.name, None);
        assert_eq!(rule.from, ["a", "b@x.org"]);
        let rule = Form::from_rule(&base).build(&base, " Mine ", "");
        assert_eq!(rule.name.as_deref(), Some("Mine"));
        assert!(rule.from.is_empty());
    }

    #[test]
    fn a_vote_rule_needs_a_label_and_a_value_or_any() {
        let mut form = Form::from_rule(&vote_rule(&[], &[]));
        assert_eq!(form.problem(), None);
        form.any_label = false;
        assert!(form.problem().expect("problem").contains("label"));
        form.labels.insert("Verified".into());
        assert_eq!(form.problem(), None);
        form.any_value = false;
        assert!(form.problem().expect("problem").contains("value"));
        form.values.insert(-1);
        assert_eq!(form.problem(), None);
        // Other events have neither.
        form.values.clear();
        form.event = RuleEvent::Merged;
        assert_eq!(form.problem(), None);
    }

    #[test]
    fn the_scope_choices_list_the_queries_and_keep_an_unknown_one() {
        let queries = vec!["Team".to_owned(), "Mine".to_owned()];
        let scopes = scope_choices(&queries, &Scope::Owner);
        assert_eq!(scopes.len(), 8);
        assert_eq!(scopes[6], Scope::Query("Team".into()));
        let gone = Scope::Query("Gone".into());
        let scopes = scope_choices(&queries, &gone);
        assert_eq!(scopes.len(), 9);
        assert_eq!(scopes.last(), Some(&gone));
        // A known query is not listed twice.
        assert_eq!(
            scope_choices(&queries, &Scope::Query("Team".into())).len(),
            8
        );
    }

    #[test]
    fn every_event_has_a_label() {
        for e in EVENTS {
            assert!(!event_label(e).is_empty());
        }
        assert_eq!(
            EVENTS
                .iter()
                .map(|e| e.key())
                .collect::<BTreeSet<_>>()
                .len(),
            10
        );
    }
}
