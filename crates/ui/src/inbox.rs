//! The notification inbox: what is kept of the events that matched a rule,
//! and the changes the page and the bell make to it. The rows live in the
//! store; the shell applies [`InboxOp`]s on the background executor, in
//! order, and reads the counts back for the bell. The bell counts the
//! notifications that are unread and not yet seen: opening the page marks
//! them all seen (the badge resets), but nothing read (the rows keep their
//! unread dots).

use std::collections::HashMap;

use jiff::Timestamp;
use store::{InboxId, NewInboxItem, ServerId, Store};
use sync::notify::{Delivered, NotificationSettings, RuleEvent};

/// What the inbox page asks the store to change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxOp {
    Read(Vec<InboxId>),
    Unread(Vec<InboxId>),
    /// Every notification of every server.
    ReadAll,
    Dismiss(Vec<InboxId>),
    /// Dismiss every read notification of every server.
    ClearRead,
    /// The page was opened (or showed new rows): every notification of
    /// every server is seen, which resets the bell but leaves the rows
    /// unread.
    Seen,
    /// Change nothing; read the counts (and the page's rows) again.
    Reload,
}

impl InboxOp {
    /// Applies the op as of `now`.
    pub fn apply(&self, store: &mut Store, now: Timestamp) -> Result<(), store::Error> {
        match self {
            Self::Read(ids) => store.mark_inbox_read(ids, now),
            Self::Unread(ids) => store.mark_inbox_unread(ids),
            Self::ReadAll => store.mark_all_inbox_read(None, now).map(drop),
            Self::Dismiss(ids) => store.delete_inbox(ids),
            Self::ClearRead => store.delete_read_inbox(None).map(drop),
            Self::Seen => store.mark_inbox_seen(None, now).map(drop),
            Self::Reload => Ok(()),
        }
    }
}

/// The inbox items for what a pass delivered, with the title of the rule
/// that fired as it is now: the rule list can be reordered later, so the
/// index is not kept.
pub fn items_for(settings: &NotificationSettings, delivered: &[Delivered]) -> Vec<NewInboxItem> {
    delivered
        .iter()
        .map(|d| {
            let rule = settings.rules.get(d.rule_index);
            NewInboxItem {
                event: d.event.clone(),
                rule_title: rule.map_or_else(|| "Notification rule".to_owned(), |r| r.title()),
                mention: rule.is_some_and(|r| r.event == RuleEvent::Mention),
                shown_on_os: d.shown_on_os,
            }
        })
        .collect()
}

/// The unread notifications of all servers that were not seen yet (the
/// bell's badge).
pub fn total(counts: &HashMap<ServerId, usize>) -> usize {
    counts.values().sum()
}

#[cfg(test)]
mod tests {
    use model::{ChangeNumber, Event, EventKind};
    use sync::notify::{CATCH_UP_LIMIT, Rule, Scope, Screen, plan};

    use super::*;

    fn event(change: u64, at: &str) -> Event {
        Event {
            server: ServerId(1),
            change: ChangeNumber(change),
            project: "p".into(),
            subject: "s".into(),
            owner: None,
            actor: None,
            at: at.parse().expect("ts"),
            kind: EventKind::Merged,
            subject_account: None,
        }
    }

    fn settings() -> NotificationSettings {
        let mut named = Rule::new(RuleEvent::Merged, Scope::Owner);
        named.name = Some("My merges".into());
        let mention = Rule::new(RuleEvent::Mention, Scope::Owner);
        NotificationSettings {
            rules: vec![named, mention],
            ..Default::default()
        }
    }

    #[test]
    fn a_catch_up_summary_still_gives_one_item_per_event() {
        let settings = settings();
        let matched: Vec<_> = (0..CATCH_UP_LIMIT as u64 + 3)
            .map(|n| (event(n, "2024-02-01T10:00:00Z"), 0))
            .collect();
        let n = matched.len();
        let screen = Screen {
            focused: false,
            current: None,
        };
        let planned = plan(
            &settings,
            "a",
            matched,
            &screen,
            jiff::civil::time(12, 0, 0, 0),
            "2024-03-01T00:00:00Z".parse().expect("ts"),
        );
        assert_eq!(planned.notifications.len(), 1);
        let items = items_for(&settings, &planned.delivered);
        assert_eq!(items.len(), n);
        assert!(
            items
                .iter()
                .all(|i| i.shown_on_os && i.rule_title == "My merges")
        );
    }

    #[test]
    fn the_title_is_kept_as_it_is_now_and_a_missing_rule_gets_a_stand_in() {
        let settings = settings();
        let delivered = |rule_index| Delivered {
            event: event(1, "2024-02-01T10:00:00Z"),
            rule_index,
            shown_on_os: false,
        };
        let items = items_for(&settings, &[delivered(1), delivered(7)]);
        assert!(items[0].mention && !items[0].shown_on_os);
        assert_eq!(items[0].rule_title, settings.rules[1].title());
        assert!(!items[1].mention);
        assert_eq!(items[1].rule_title, "Notification rule");
    }
}
