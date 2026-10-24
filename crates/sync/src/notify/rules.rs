//! The rule model and the global notification settings.

use std::time::Duration;

use jiff::civil::Time;

/// The kind of event a rule is about. Mirrors [`model::EventKind`], plus
/// [`RuleEvent::Mention`], which is a comment that mentions the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleEvent {
    Vote,
    Comment,
    AttentionAdded,
    ReviewerAdded,
    PatchSet,
    Merged,
    Abandoned,
    ReadyToSubmit,
    MergeConflict,
    Mention,
}

impl RuleEvent {
    /// The spelling in the config file.
    pub fn key(self) -> &'static str {
        match self {
            Self::Vote => "vote",
            Self::Comment => "comment",
            Self::AttentionAdded => "attention_added",
            Self::ReviewerAdded => "reviewer_added",
            Self::PatchSet => "patch_set",
            Self::Merged => "merged",
            Self::Abandoned => "abandoned",
            Self::ReadyToSubmit => "ready_to_submit",
            Self::MergeConflict => "merge_conflict",
            Self::Mention => "mention",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "vote" => Self::Vote,
            "comment" => Self::Comment,
            "attention_added" => Self::AttentionAdded,
            "reviewer_added" => Self::ReviewerAdded,
            "patch_set" => Self::PatchSet,
            "merged" => Self::Merged,
            "abandoned" => Self::Abandoned,
            "ready_to_submit" => Self::ReadyToSubmit,
            "merge_conflict" => Self::MergeConflict,
            "mention" => Self::Mention,
            _ => return None,
        })
    }

    /// The event as a phrase for a list, without the scope.
    pub fn title(self) -> &'static str {
        match self {
            Self::Vote => "Vote",
            Self::Comment => "Comment",
            Self::AttentionAdded => "Added to the attention set",
            Self::ReviewerAdded => "Added as reviewer or CC",
            Self::PatchSet => "New patch set",
            Self::Merged => "Merged",
            Self::Abandoned => "Abandoned",
            Self::ReadyToSubmit => "Ready to submit",
            Self::MergeConflict => "Merge conflict",
            Self::Mention => "Mentioned in a comment",
        }
    }

    /// Whether the rule needs changes that are no longer open: the event
    /// is the change closing.
    pub fn needs_closed_changes(self) -> bool {
        matches!(self, Self::Merged | Self::Abandoned)
    }
}

/// Which changes a rule applies to, judged from the cached change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The signed-in user owns the change.
    Owner,
    Reviewer,
    Cc,
    /// The user is in the attention set.
    Attention,
    Starred,
    /// Any of the above.
    Any,
    /// The change is in the cached result of the saved query with this name.
    Query(String),
}

impl Scope {
    /// The spelling of the simple scopes in the config file; `None` for
    /// [`Scope::Query`], which has its own key.
    pub fn key(&self) -> Option<&'static str> {
        match self {
            Self::Owner => Some("owner"),
            Self::Reviewer => Some("reviewer"),
            Self::Cc => Some("cc"),
            Self::Attention => Some("attention"),
            Self::Starred => Some("starred"),
            Self::Any => Some("any"),
            Self::Query(_) => None,
        }
    }

    /// The scope as the end of a sentence: "on my changes".
    pub fn describe(&self) -> String {
        match self {
            Self::Owner => "on my changes".to_owned(),
            Self::Reviewer => "on changes I review".to_owned(),
            Self::Cc => "on changes I am CC'd on".to_owned(),
            Self::Attention => "on changes in my attention set".to_owned(),
            Self::Starred => "on starred changes".to_owned(),
            Self::Any => "on any change".to_owned(),
            Self::Query(name) => format!("on changes of the query \u{201c}{name}\u{201d}"),
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "owner" => Self::Owner,
            "reviewer" => Self::Reviewer,
            "cc" => Self::Cc,
            "attention" => Self::Attention,
            "starred" => Self::Starred,
            "any" => Self::Any,
            _ => return None,
        })
    }
}

/// One notification rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub name: Option<String>,
    pub event: RuleEvent,
    pub scope: Scope,
    /// Slugs of the servers the rule applies to; empty: all of them.
    pub servers: Vec<String>,
    /// Vote rules: the label names (exact, case-sensitive), or `"*"` for
    /// any. Empty means any label.
    pub labels: Vec<String>,
    /// Vote rules: the exact values to report. A retraction is the value
    /// 0, so it only matches a rule that lists 0. Empty means any value.
    pub values: Vec<i32>,
    /// Comment rules: only comments of unresolved threads. Carried, not
    /// evaluated: that needs thread data the event does not have. It
    /// documents the user's intent until the matcher gets that data.
    pub only_unresolved: bool,
    /// Comment rules: only replies to the user's own comments. Carried,
    /// not evaluated, for the same reason as `only_unresolved`.
    pub replies_to_me: bool,
    /// Skip events whose actor is a bot (see [`MatchContext`](super::MatchContext)).
    pub exclude_bots: bool,
    /// Only these actors, by username, name, display name or email,
    /// case-insensitive. Empty: any actor.
    pub from: Vec<String>,
    pub enabled: bool,
}

impl Rule {
    /// A rule for `event` on changes in `scope` with every other option at
    /// its default.
    pub fn new(event: RuleEvent, scope: Scope) -> Self {
        Self {
            name: None,
            event,
            scope,
            servers: Vec::new(),
            labels: Vec::new(),
            values: Vec::new(),
            only_unresolved: false,
            replies_to_me: false,
            exclude_bots: true,
            from: Vec::new(),
            enabled: true,
        }
    }
}

impl Rule {
    /// What the rule does in a few words, like "Vote Code-Review -2, -1 on
    /// my changes". Does not use the name.
    pub fn describe(&self) -> String {
        let mut text = match self.event {
            RuleEvent::Vote => {
                let any_label = self.labels.is_empty() || self.labels.iter().any(|l| l == "*");
                match (any_label, self.values.is_empty()) {
                    (true, true) => "Any vote".to_owned(),
                    (any, no_values) => {
                        let labels = if any {
                            "any label".to_owned()
                        } else {
                            self.labels.join(", ")
                        };
                        let values = if no_values {
                            "any value".to_owned()
                        } else {
                            let signed = |v: &i32| {
                                if *v == 0 {
                                    "0".to_owned()
                                } else {
                                    format!("{v:+}")
                                }
                            };
                            let values: Vec<_> = self.values.iter().map(signed).collect();
                            values.join(", ")
                        };
                        format!("Vote {labels} {values}")
                    }
                }
            }
            other => other.title().to_owned(),
        };
        text.push(' ');
        text.push_str(&self.scope.describe());
        if !self.from.is_empty() {
            text.push_str(" from ");
            text.push_str(&self.from.join(", "));
        }
        text
    }

    /// The name if the rule has one, else [`Rule::describe`].
    pub fn title(&self) -> String {
        self.name
            .as_deref()
            .filter(|n| !n.trim().is_empty())
            .map_or_else(|| self.describe(), str::to_owned)
    }
}

/// The rules in effect when the config file has none: my attention set,
/// negative votes on my changes, new comments on my changes.
pub fn default_rules() -> Vec<Rule> {
    let named = |name: &str, mut rule: Rule| {
        rule.name = Some(name.to_owned());
        rule
    };
    let vote = |label: &str, values: &[i32]| {
        let mut rule = Rule::new(RuleEvent::Vote, Scope::Owner);
        rule.labels = vec![label.to_owned()];
        rule.values = values.to_vec();
        rule
    };
    vec![
        named(
            "Added to the attention set",
            Rule::new(RuleEvent::AttentionAdded, Scope::Attention),
        ),
        named(
            "Negative Code-Review on my changes",
            vote("Code-Review", &[-2, -1]),
        ),
        named("Negative Verified on my changes", vote("Verified", &[-1])),
        named(
            "Comments on my changes",
            Rule::new(RuleEvent::Comment, Scope::Owner),
        ),
    ]
}

/// A daily time window, in local time, in which nothing is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuietHours {
    pub start: Time,
    /// Exclusive. Before `start` means the window wraps past midnight.
    pub end: Time,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("quiet hours must look like \"22:00-07:00\", not {0:?}")]
pub struct QuietHoursError(pub String);

impl QuietHours {
    /// Parses `"HH:MM-HH:MM"`. Equal ends make an empty window.
    pub fn parse(text: &str) -> Result<Self, QuietHoursError> {
        let err = || QuietHoursError(text.to_owned());
        let (a, b) = text.split_once('-').ok_or_else(err)?;
        let time = |s: &str| -> Option<Time> {
            let (h, m) = s.trim().split_once(':')?;
            if h.len() != 2 || m.len() != 2 {
                return None;
            }
            Time::new(h.parse().ok()?, m.parse().ok()?, 0, 0).ok()
        };
        Ok(Self {
            start: time(a).ok_or_else(err)?,
            end: time(b).ok_or_else(err)?,
        })
    }

    /// `"22:00-07:00"`, the form [`QuietHours::parse`] reads.
    pub fn format(&self) -> String {
        format!(
            "{:02}:{:02}-{:02}:{:02}",
            self.start.hour(),
            self.start.minute(),
            self.end.hour(),
            self.end.minute()
        )
    }

    /// Whether `now` is inside the window; the start is inside, the end is
    /// not.
    pub fn contains(&self, now: Time) -> bool {
        use std::cmp::Ordering::*;
        match self.start.cmp(&self.end) {
            Equal => false,
            Less => self.start <= now && now < self.end,
            Greater => now >= self.start || now < self.end,
        }
    }
}

/// The `[notifications]` section, resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationSettings {
    /// The master switch: off, nothing matches.
    pub enabled: bool,
    pub only_when_unfocused: bool,
    pub suppress_current_change: bool,
    /// Drop events caused by the signed-in user.
    pub ignore_own_actions: bool,
    pub quiet_hours: Option<QuietHours>,
    /// Events within this window of each other are shown as one.
    pub group_window: Duration,
    pub rules: Vec<Rule>,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            only_when_unfocused: true,
            suppress_current_change: true,
            ignore_own_actions: true,
            quiet_hours: None,
            group_window: Duration::from_secs(20),
            rules: default_rules(),
        }
    }
}

impl NotificationSettings {
    /// Whether `now` (local time, passed in) falls in the quiet hours.
    pub fn in_quiet_hours(&self, now: Time) -> bool {
        self.quiet_hours.is_some_and(|q| q.contains(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vote(labels: &[&str], values: &[i32]) -> Rule {
        let mut r = Rule::new(RuleEvent::Vote, Scope::Owner);
        r.labels = labels.iter().map(|l| (*l).to_owned()).collect();
        r.values = values.to_vec();
        r
    }

    #[test]
    fn a_vote_rule_names_its_labels_and_values() {
        assert_eq!(
            vote(&["Code-Review"], &[-2, -1]).describe(),
            "Vote Code-Review -2, -1 on my changes"
        );
        assert_eq!(
            vote(&["Code-Review", "Verified"], &[1, 2]).describe(),
            "Vote Code-Review, Verified +1, +2 on my changes"
        );
        assert_eq!(
            vote(&["Verified"], &[0]).describe(),
            "Vote Verified 0 on my changes"
        );
    }

    #[test]
    fn any_label_and_any_value_are_said_so() {
        assert_eq!(vote(&[], &[]).describe(), "Any vote on my changes");
        assert_eq!(vote(&["*"], &[]).describe(), "Any vote on my changes");
        assert_eq!(
            vote(&["Verified"], &[]).describe(),
            "Vote Verified any value on my changes"
        );
        assert_eq!(
            vote(&[], &[-1]).describe(),
            "Vote any label -1 on my changes"
        );
    }

    #[test]
    fn other_events_read_as_a_phrase_with_their_scope_and_actors() {
        let r = Rule::new(RuleEvent::AttentionAdded, Scope::Attention);
        assert_eq!(
            r.describe(),
            "Added to the attention set on changes in my attention set"
        );
        let mut r = Rule::new(RuleEvent::Comment, Scope::Query("Team".into()));
        r.from = vec!["alice".into(), "bob".into()];
        assert_eq!(
            r.describe(),
            "Comment on changes of the query \u{201c}Team\u{201d} from alice, bob"
        );
        assert_eq!(
            Rule::new(RuleEvent::Merged, Scope::Starred).describe(),
            "Merged on starred changes"
        );
    }

    #[test]
    fn the_title_is_the_name_unless_it_is_blank() {
        let mut r = Rule::new(RuleEvent::Merged, Scope::Owner);
        assert_eq!(r.title(), "Merged on my changes");
        r.name = Some("  ".into());
        assert_eq!(r.title(), "Merged on my changes");
        r.name = Some("Shipped".into());
        assert_eq!(r.title(), "Shipped");
    }
}
