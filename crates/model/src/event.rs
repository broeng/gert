use serde::{Deserialize, Serialize};

use crate::{Account, AccountId, ChangeNumber, PatchSetNumber, ServerId, Timestamp};

/// The longest message text an [`EventKind::Comment`] carries, in
/// characters.
pub const COMMENT_SNIPPET_CHARS: usize = 500;

/// What happened on a change, with only the data the rule matcher and the
/// notification text need.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    /// A vote changed. `value` 0 means the vote was removed; `previous` is
    /// the old value, `None` if the account had not voted.
    Vote {
        label: String,
        value: i32,
        previous: Option<i32>,
    },
    /// A new change message with text of its own. The header line
    /// (`Patch Set 2: Code-Review+1`) is already stripped, and the text is
    /// cut at [`COMMENT_SNIPPET_CHARS`]. Whether it mentions the signed-in
    /// user is for the matcher to decide.
    Comment {
        message_id: String,
        snippet: String,
    },
    /// `subject_account` entered the attention set.
    AttentionAdded,
    /// `subject_account` was added as a reviewer, or as CC.
    ReviewerAdded {
        cc: bool,
    },
    NewPatchSet {
        number: PatchSetNumber,
    },
    Merged,
    Abandoned,
    /// The change became submittable.
    ReadyToSubmit,
    /// The change stopped being mergeable.
    MergeConflict,
}

impl EventKind {
    /// A short stable name, also used in [`Event::key`].
    pub fn name(&self) -> &'static str {
        match self {
            Self::Vote { .. } => "vote",
            Self::Comment { .. } => "comment",
            Self::AttentionAdded => "attention",
            Self::ReviewerAdded { .. } => "reviewer",
            Self::NewPatchSet { .. } => "patchset",
            Self::Merged => "merged",
            Self::Abandoned => "abandoned",
            Self::ReadyToSubmit => "ready",
            Self::MergeConflict => "conflict",
        }
    }
}

/// Something that happened on a change, derived from two cached states of
/// it. The change facts are copied in so that an event can be shown without
/// looking the change up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub server: ServerId,
    pub change: ChangeNumber,
    pub project: String,
    pub subject: String,
    pub owner: Option<Account>,
    /// Who did it, when the change data says. Not known for changes to the
    /// attention set or reviewer list, nor for a merge.
    pub actor: Option<Account>,
    pub at: Timestamp,
    pub kind: EventKind,
    /// The account the event is about: the added reviewer, the account that
    /// entered the attention set. `None` for events about the change as a
    /// whole. (For a vote the actor is the voter.)
    pub subject_account: Option<AccountId>,
}

impl Event {
    /// A stable identity for deduplication: the same happening yields the
    /// same key every time it is detected, on any run. It names the server,
    /// change, kind, the kind's own data (patch set, label, value, message
    /// id), the accounts involved and the time.
    pub fn key(&self) -> String {
        let id = |a: Option<AccountId>| a.map_or_else(|| "-".to_owned(), |a| a.0.to_string());
        let mut key = format!("{}/{}/{}", self.server.0, self.change.0, self.kind.name());
        match &self.kind {
            EventKind::Vote {
                label,
                value,
                previous,
            } => {
                let prev = previous.map_or_else(|| "-".to_owned(), |p| p.to_string());
                key.push_str(&format!("/{label}/{value}/{prev}"));
            }
            EventKind::Comment { message_id, .. } => key.push_str(&format!("/{message_id}")),
            EventKind::ReviewerAdded { cc } => key.push_str(if *cc { "/cc" } else { "/reviewer" }),
            EventKind::NewPatchSet { number } => key.push_str(&format!("/{number}")),
            EventKind::AttentionAdded
            | EventKind::Merged
            | EventKind::Abandoned
            | EventKind::ReadyToSubmit
            | EventKind::MergeConflict => {}
        }
        key.push_str(&format!(
            "/{}/{}/{}",
            id(self.actor.as_ref().map(|a| a.id)),
            id(self.subject_account),
            self.at.as_millisecond()
        ));
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: u64) -> Account {
        Account {
            id: AccountId(id),
            name: None,
            email: None,
            username: None,
            display_name: None,
        }
    }

    fn event(kind: EventKind) -> Event {
        Event {
            server: ServerId(2),
            change: ChangeNumber(77),
            project: "p".into(),
            subject: "s".into(),
            owner: Some(account(1)),
            actor: Some(account(5)),
            at: "2024-03-01T10:00:00Z".parse().unwrap(),
            kind,
            subject_account: None,
        }
    }

    fn vote(label: &str, value: i32, previous: Option<i32>) -> EventKind {
        EventKind::Vote {
            label: label.into(),
            value,
            previous,
        }
    }

    #[test]
    fn key_is_stable_and_readable() {
        assert_eq!(
            event(vote("Code-Review", -1, Some(1))).key(),
            "2/77/vote/Code-Review/-1/1/5/-/1709287200000"
        );
        assert_eq!(
            event(EventKind::NewPatchSet {
                number: PatchSetNumber(3)
            })
            .key(),
            "2/77/patchset/3/5/-/1709287200000"
        );
        let mut e = event(EventKind::AttentionAdded);
        e.actor = None;
        e.subject_account = Some(AccountId(9));
        assert_eq!(e.key(), "2/77/attention/-/9/1709287200000");
    }

    #[test]
    fn key_ignores_display_data_but_not_identity() {
        let a = event(EventKind::Merged);
        let mut b = a.clone();
        b.subject = "renamed".into();
        b.project = "q".into();
        b.owner = None;
        assert_eq!(a.key(), b.key());

        let mut c = a.clone();
        c.server = ServerId(3);
        let mut d = a.clone();
        d.change = ChangeNumber(78);
        let mut e = a.clone();
        e.at = "2024-03-01T10:00:01Z".parse().unwrap();
        let mut f = a.clone();
        f.actor = Some(account(6));
        let keys = [a.key(), c.key(), d.key(), e.key(), f.key()];
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(keys.iter().filter(|o| *o == k).count(), 1, "{i}: {k}");
        }
    }

    #[test]
    fn key_distinguishes_kind_data() {
        let kinds = [
            vote("Code-Review", -1, None),
            vote("Code-Review", -1, Some(1)),
            vote("Code-Review", -2, None),
            vote("Verified", -1, None),
            EventKind::Comment {
                message_id: "m1".into(),
                snippet: "x".into(),
            },
            EventKind::Comment {
                message_id: "m2".into(),
                snippet: "x".into(),
            },
            EventKind::ReviewerAdded { cc: false },
            EventKind::ReviewerAdded { cc: true },
            EventKind::NewPatchSet {
                number: PatchSetNumber(2),
            },
            EventKind::NewPatchSet {
                number: PatchSetNumber(3),
            },
            EventKind::Merged,
            EventKind::Abandoned,
            EventKind::ReadyToSubmit,
            EventKind::MergeConflict,
            EventKind::AttentionAdded,
        ];
        let keys: Vec<_> = kinds.iter().cloned().map(|k| event(k).key()).collect();
        for k in &keys {
            assert_eq!(keys.iter().filter(|o| *o == k).count(), 1, "{k}");
        }
        // The snippet is display data.
        let a = event(EventKind::Comment {
            message_id: "m1".into(),
            snippet: "one".into(),
        });
        let b = event(EventKind::Comment {
            message_id: "m1".into(),
            snippet: "two".into(),
        });
        assert_eq!(a.key(), b.key());
    }

    #[test]
    fn events_round_trip_through_json() {
        let e = event(vote("Code-Review", 2, None));
        let back: Event = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back, e);
    }
}
